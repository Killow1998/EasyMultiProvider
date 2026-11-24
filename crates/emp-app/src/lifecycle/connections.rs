use crate::app::ServerState;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::net::TcpListener;
use std::sync::Arc;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

pub(crate) const ACCEPT_RETRY: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct AcceptLog(Option<Instant>);

impl AcceptLog {
    pub(crate) fn record<T>(&mut self, state: &ServerState, result: &std::io::Result<T>) {
        match result {
            Ok(_) => {
                if self.0.take().is_some() {
                    state.backend.diagnostics.journal.event(
                        "info",
                        "listener_accept_recovered",
                        &serde_json::json!({}),
                    );
                }
            }
            Err(error)
                if error.kind() != std::io::ErrorKind::Interrupted
                    && self
                        .0
                        .is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) =>
            {
                state.backend.diagnostics.journal.event(
                    "warning",
                    "listener_accept_failed",
                    &serde_json::json!({
                        "os_error":error.raw_os_error(), "error_kind":format!("{:?}", error.kind()),
                        "retry_ms":ACCEPT_RETRY.as_millis() as u64,
                    }),
                );
                self.0 = Some(Instant::now());
            }
            Err(_) => {}
        }
    }
}

pub(crate) fn request_rejected(state: &ServerState, reason: &'static str) {
    state.backend.diagnostics.journal.event(
        "warning",
        "request_rejected",
        &serde_json::json!({"transport":"http", "reason":reason}),
    );
}

pub(crate) fn spawn_request(
    state: &Arc<ServerState>,
    work: impl FnOnce(&Arc<ServerState>) + Send + 'static,
) {
    let request_state = Arc::clone(state);
    if thread::Builder::new()
        .name("emp-request".to_owned())
        .spawn(move || work(&request_state))
        .is_err()
    {
        request_rejected(state, "worker_spawn_failed");
    }
}

#[cfg(any(test, not(feature = "server-candidate")))]
pub(super) fn accept(listener: TcpListener, state: Arc<ServerState>) {
    let mut log = AcceptLog::default();
    while !state.shutdown.load(Ordering::Acquire) {
        let accepted = listener.accept();
        log.record(&state, &accepted);
        let (stream, _) = match accepted {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "accept 遇到资源耗尽等错误后按既有固定间隔退避"
                )]
                thread::sleep(ACCEPT_RETRY);
                continue;
            }
        };
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        let Some(request_permit) = state.connection_admission.acquire_request() else {
            request_rejected(&state, "connection_capacity");
            continue;
        };
        spawn_request(&state, move |state| {
            crate::http::connection::handle_connection(
                stream,
                state,
                request_permit,
                None,
                crate::services::observation::http::HttpObservation::new(state),
            );
        });
    }
}
