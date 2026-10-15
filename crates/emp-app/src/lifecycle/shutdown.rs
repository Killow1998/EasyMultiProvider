//! Restore configuration before draining workers or returning from OS exit events.
use super::ServerHandle;
use crate::app::ServerState;
use crate::error::AppError;
use std::net::TcpStream;
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
    state.connection_admission.close();
    state.request_shutdown();
    if preserve_integration {
        return Ok(());
    }
    state.backend.integration.restore_owned_on_exit()
}

/// Also runs on a startup error or Rust stack unwind, while this process still
/// owns its files. No other process or later startup performs this cleanup.
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
        self.shutdown_with_handoff(false)
    }

    pub(super) fn shutdown_with_handoff(self, preserve_integration: bool) -> Result<(), AppError> {
        let diagnostics = Arc::clone(&self.state.backend.diagnostics);
        diagnostics
            .journal
            .event("info", "shutdown_start", &serde_json::json!({}));
        let result = self.shutdown_inner(preserve_integration);
        diagnostics.journal.event(
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
        result
    }

    fn shutdown_inner(self, preserve_integration: bool) -> Result<(), AppError> {
        // Routing must be native before worker/credential drains. Windows can
        // terminate us after its console-close deadline even if work is busy.
        let restoration = prepare_exit(&self.state, preserve_integration);
        self.state.catalog_refresh.stop();
        // Restore first, then allow admitted handlers to flush their final
        // receipts before process exit. OS exit callbacks only run prepare_exit.
        let admitted_work_drained = if let Some(gate) = self
            .state
            .connection_admission
            .quiesce(0, Duration::from_secs(30))
        {
            gate.keep_closed();
            true
        } else {
            false
        };
        let restoration = restoration.and(if admitted_work_drained {
            Ok(())
        } else {
            Err(AppError::ServerStopped)
        });
        // Request threads are not joined (streams may outlive shutdown), so
        // quota checks that may rotate a credential are drained explicitly
        // before the final save below.
        let credential_operations = &self.state.backend.accounts.credential_operations;
        let _ = credential_operations.close_and_drain(Duration::ZERO);
        let _ = TcpStream::connect_timeout(&self.local_addr, Duration::from_millis(100));
        self.state.backend.usage.stop();
        let accounts = &self.state.backend.accounts;
        // The same mutex must cover the condition check and notification,
        // otherwise shutdown can arrive just before a worker starts waiting.
        self.state.backend.management_events.close();
        if let Ok(_wait) = accounts.quota_sampler_wait.lock() {
            accounts.quota_sampler_condition.notify_all();
        }
        crate::services::runtime::stop_watch(&self.state);
        for worker in self.workers {
            let _: () = worker.join().map_err(|_| AppError::ServerStopped)?;
        }
        // Last chance to save credentials Codex rotated: the stored copies
        // may already be invalid upstream.
        // A shutdown that loses them is not a clean exit.
        let unfinished = credential_operations.close_and_drain(CREDENTIAL_DRAIN_TIMEOUT);
        let unsaved = unfinished + crate::services::quota::flush_pending_rotations(&self.state);
        drop(self._service_owner);
        if unsaved == 0 {
            return restoration;
        }
        let unsaved = AppError::CredentialsUnsaved(unsaved);
        match restoration {
            Ok(()) => Err(unsaved),
            Err(error) => {
                // Only one error is returned; do not let it hide this one.
                eprintln!("{unsaved}");
                Err(error)
            }
        }
    }
}
