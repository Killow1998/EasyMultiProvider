//! Restore configuration before draining workers or returning from OS exit events.
use super::ServerHandle;
use crate::app::ServerState;
use crate::error::AppError;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
mod tests;

/// Two 45 s quota queries plus credential-save retries.
const CREDENTIAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(100);

pub(super) fn prepare_exit(
    state: &ServerState,
    preserve_integration: bool,
) -> Result<(), AppError> {
    state.request_shutdown();
    if preserve_integration {
        return Ok(());
    }
    state.backend.integration.restore_owned_on_exit()
}

#[cfg(any(windows, test))]
pub(super) fn confirm_session_exit_restore(state: &ServerState, result: Result<(), AppError>) {
    state.backend.diagnostics.journal.event_and_flush_blocking(
        if result.is_ok() { "info" } else { "error" },
        "session_exit_restore",
        &serde_json::json!({"success": result.is_ok()}),
    );
}

/// Restores this process's owned configuration on startup failure or stack unwind.
pub(super) struct RestoreBeforeExit {
    state: Arc<ServerState>,
    armed: bool,
}

impl RestoreBeforeExit {
    pub(super) fn new(state: Arc<ServerState>) -> Self {
        Self { state, armed: true }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RestoreBeforeExit {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = prepare_exit(&self.state, false)
        {
            eprintln!("EMP could not restore native configuration before exit: {error}");
        }
    }
}

impl ServerHandle {
    pub fn shutdown(self) -> Result<(), AppError> {
        self.shutdown_with_handoff(false, || {})
    }

    pub(super) fn shutdown_with_handoff(
        mut self,
        preserve_integration: bool,
        close_exit_callbacks: impl FnOnce(),
    ) -> Result<(), AppError> {
        self.state.backend.diagnostics.journal.event(
            "info",
            "shutdown_start",
            &serde_json::json!({}),
        );
        let result = self.shutdown_inner(preserve_integration);
        close_exit_callbacks();
        self.state
            .backend
            .diagnostics
            .journal
            .close_with_event_blocking(
                if result.is_ok() { "info" } else { "error" },
                "shutdown_complete",
                &serde_json::json!({
                    "success": result.is_ok(),
                    "error_class": match &result {
                        Ok(()) => "none",
                        Err(AppError::CredentialsUnsaved(_)) => "credentials_unsaved",
                        Err(AppError::NativeRestoreBlocked(_)) => "native_restore_blocked",
                        Err(_) => "shutdown_failed",
                    },
                }),
            );
        drop(self.service_owner);
        result
    }

    fn shutdown_inner(&mut self, preserve_integration: bool) -> Result<(), AppError> {
        let restoration = prepare_exit(&self.state, preserve_integration);
        self.state.catalog_refresh.stop();
        let listener = self.listener.take().map_or(Ok(()), |listener| {
            listener
                .join()
                .map_err(|_| AppError::ServerStopped)?
                .map_err(|_| AppError::ServerStopped)
        });
        let restoration = restoration.and(listener);
        let credential_operations = &self.state.backend.accounts.credential_operations;
        let _ = credential_operations.close_and_drain(Duration::ZERO);
        self.state.backend.usage.stop();
        let accounts = &self.state.backend.accounts;
        if let Ok(_wait) = accounts.quota_sampler_wait.lock() {
            accounts.quota_sampler_condition.notify_all();
        }
        crate::services::runtime::stop_watch(&self.state);
        for worker in self.workers.drain(..) {
            let _: () = worker.join().map_err(|_| AppError::ServerStopped)?;
        }
        self.state.backend.modules.stop();
        let unfinished = credential_operations.close_and_drain(CREDENTIAL_DRAIN_TIMEOUT);
        let unsaved = unfinished + crate::services::quota::flush_pending_rotations(&self.state);
        if unsaved == 0 {
            return restoration;
        }
        let unsaved = AppError::CredentialsUnsaved(unsaved);
        match restoration {
            Ok(()) => Err(unsaved),
            Err(error) => {
                eprintln!("{unsaved}");
                Err(error)
            }
        }
    }
}
