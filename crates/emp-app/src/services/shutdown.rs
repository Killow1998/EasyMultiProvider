//! Shutdown preparation owns update, active-work and integration constraints.
use crate::app::ServerState;
use crate::services::observation::operation::observe;
use std::time::Duration;

pub(crate) struct ShutdownError {
    pub(crate) code: Option<&'static str>,
    pub(crate) message: &'static str,
}

pub(crate) fn prepare_quit(
    request_id: Option<&str>,
    state: &ServerState,
) -> Result<(), ShutdownError> {
    observe(&state.backend.diagnostics, request_id, "quit", |receipt| {
        if matches!(
            state.updates.snapshot().state.as_str(),
            "downloading" | "verifying" | "waiting" | "installing"
        ) {
            return Err(ShutdownError {
                code: None,
                message: "Wait for the update to finish before exiting EMP",
            });
        }
        let Some(restore_gate) = state
            .connection_admission
            .quiesce(1, Duration::from_secs(15))
        else {
            return Err(ShutdownError {
                code: Some("active_conversations"),
                message: "Finish active Codex requests and WebSockets, then retry shutdown",
            });
        };
        if let Err(error) = receipt.step("restore_owned_configuration", || {
            state.backend.integration.restore_owned()
        }) {
            let (code, message) = match error {
                crate::error::AppError::NativeRestoreBlocked(reason) => (
                    Some(reason),
                    crate::services::integration::restore_error_message(reason),
                ),
                _ => (
                    None,
                    "Native configuration could not be restored; EMP is still running",
                ),
            };
            return Err(ShutdownError { code, message });
        }
        restore_gate.keep_closed();
        receipt.fact("service_shutdown", "ready_after_response");
        receipt.check("desktop_effect_verified", None);
        Ok(())
    })
}
