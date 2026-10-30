//! Accept and admit HTTP connections; request handling owns the admitted work.

use crate::app::ServerState;
use crate::http::connection::handle_connection;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);

pub(super) fn accept(listener: TcpListener, state: Arc<ServerState>) {
    let mut last_error = None;
    while !state.shutdown.load(Ordering::Acquire) {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => {
                if last_error.take().is_some() {
                    state.backend.diagnostics.journal.event(
                        "info",
                        "listener_accept_recovered",
                        &serde_json::json!({}),
                    );
                }
                accepted
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                // EMFILE/ENFILE and transient accept errors must not silently
                // drop the listener while existing WebSockets keep running.
                // Rate-limit diagnostics and back off instead of spinning.
                if last_error.is_none_or(|at: Instant| at.elapsed() >= Duration::from_secs(5)) {
                    state.backend.diagnostics.journal.event("warning", "listener_accept_failed", &serde_json::json!({
                        "os_error":error.raw_os_error(), "error_kind":format!("{:?}", error.kind()),
                        "retry_ms":ACCEPT_RETRY.as_millis() as u64,
                    }));
                    last_error = Some(Instant::now());
                }
                thread::sleep(ACCEPT_RETRY);
                continue;
            }
        };
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        let Some(request_permit) = state.connection_admission.acquire_request() else {
            state.backend.diagnostics.journal.event(
                "warning",
                "request_rejected",
                &serde_json::json!({"transport":"http", "reason":"connection_capacity"}),
            );
            continue;
        };
        let request_state = Arc::clone(&state);
        if thread::Builder::new()
            .name("emp-request".to_owned())
            .spawn(move || {
                handle_connection(stream, &request_state, Some(request_permit));
            })
            .is_err()
        {
            state.backend.diagnostics.journal.event(
                "warning",
                "request_rejected",
                &serde_json::json!({"transport":"http", "reason":"worker_spawn_failed"}),
            );
        }
    }
}
