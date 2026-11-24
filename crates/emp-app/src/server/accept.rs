use crate::app::ServerState;
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
