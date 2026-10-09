//! Accept and admit HTTP connections; request handling owns the admitted work.

use crate::app::ServerState;
use crate::http::routes::handle_connection;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;

pub(super) fn accept(listener: TcpListener, state: Arc<ServerState>) {
    while !state.shutdown.load(Ordering::Acquire) {
        let Ok((stream, _)) = listener.accept() else {
            break;
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
