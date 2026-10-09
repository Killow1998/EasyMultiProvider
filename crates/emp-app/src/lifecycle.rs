//! Service ownership, background-worker registration and ordered shutdown.

mod connections;
mod startup;

use crate::app::ServerState;
use crate::error::AppError;
use crate::http::auth::codex_auth_path;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Longest a quota check can still rotate a credential: two 45 s Codex
/// queries plus the save retries.
const CREDENTIAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(100);

pub(crate) struct ServerHandle {
    local_addr: SocketAddr,
    pub(crate) state: Arc<ServerState>,
    workers: Vec<JoinHandle<()>>,
    _service_owner: emp_state::IntegrationFileLock,
}

#[derive(Clone, Default)]
struct UpdateStartupMarkers {
    ready_path: Option<PathBuf>,
    rolled_back: bool,
}

impl ServerHandle {
    fn spawn_worker(
        &mut self,
        name: &str,
        run: impl FnOnce(Arc<ServerState>) + Send + 'static,
    ) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || run(state))
            .map_err(AppError::Io)?;
        self.workers.push(worker);
        Ok(())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn bootstrap_url(&self) -> String {
        format!(
            "http://{}/?bootstrap={}",
            self.local_addr, self.state.bootstrap.token
        )
    }

    #[cfg(test)]
    pub fn session_token(&self) -> String {
        let session = self
            .state
            .sessions
            .session
            .lock()
            .expect("session lock is not poisoned");
        session.token().to_owned()
    }

    fn reconcile_startup(&self) {
        let result = crate::services::startup::reconcile(&self.state);
        self.state.backend.diagnostics.journal.event(
            if result.is_ok() { "info" } else { "warning" },
            "startup_reconcile",
            &serde_json::json!({"success": result.is_ok()}),
        );
        if result.is_err() {
            eprintln!(
                "EMP integration recovery is unavailable; inspect integration status before applying changes"
            );
        }
    }

    pub fn shutdown(self) -> Result<(), AppError> {
        let diagnostics = Arc::clone(&self.state.backend.diagnostics);
        diagnostics
            .journal
            .event("info", "shutdown_start", &serde_json::json!({}));
        let result = self.shutdown_inner();
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

    fn shutdown_inner(self) -> Result<(), AppError> {
        let installing = self.state.updates.snapshot().state == "installing";
        self.state.request_shutdown();
        self.state.catalog_refresh.stop();
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
        let restoration = if !admitted_work_drained {
            Err(AppError::ServerStopped)
        } else if installing {
            Ok(())
        } else {
            self.state.backend.integration.restore_owned()
        };
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

pub(crate) fn run_server(
    config: Option<&Path>,
    host: IpAddr,
    port: u16,
    open_browser: bool,
) -> Result<(), AppError> {
    let markers = consume_startup_markers();
    let config_path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(emp_state::config_path);
    let mut server = ServerHandle::start_with_config_options_and_browser(
        host,
        port,
        &config_path,
        "codex",
        codex_auth_path(),
        open_browser,
        markers.clone(),
    )?;
    let prepared = (|| {
        server
            .state
            .backend
            .configuration
            .set_listener(host, port)
            .map_err(|_| AppError::ServerStopped)?;
        server.reconcile_startup();
        let usage_workers = crate::services::usage::workers(&server.state)?;
        server.workers.extend(usage_workers);
        Ok::<(), AppError>(())
    })();
    if let Err(error) = prepared {
        server.state.backend.diagnostics.journal.event(
            "warning",
            "startup_failure",
            &serde_json::json!({"stage":"background_workers", "error_class":"startup_error"}),
        );
        let _ = server.shutdown();
        return Err(error);
    }
    let result = server.state.backend.transport.runtime.block_on(async {
        // Register before announcing readiness, so immediate termination is safe.
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        #[cfg(unix)]
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        #[cfg(windows)]
        let mut interrupt = tokio::signal::windows::ctrl_c()?;
        #[cfg(windows)]
        let mut terminate = tokio::signal::windows::ctrl_break()?;
        let local_addr = server.local_addr();
        println!("EMP listening on http://{local_addr}");
        println!(
            "Configuration file: {}",
            server.state.backend.configuration.config_path.display()
        );
        println!(
            "Network proxy: {}",
            server
                .state
                .backend
                .transport
                .support_network
                .source_at_startup
        );
        let bootstrap_url = server.bootstrap_url();
        println!("Open in browser: {bootstrap_url}");
        if open_browser && !crate::cli::desktop::open_browser(&bootstrap_url) {
            println!("Browser did not open automatically; use the URL above.");
        }
        emp_state::update::worker::mark_ready(crate::VERSION, markers.ready_path.clone())
            .map_err(std::io::Error::other)?;
        let requested = server.state.wait_for_shutdown();
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
            _ = requested => {},
        }
        Ok::<(), std::io::Error>(())
    });
    let cleanup = server.shutdown();
    result?;
    cleanup
}

fn consume_startup_markers() -> UpdateStartupMarkers {
    let ready_path = std::env::var_os("EMP_UPDATE_READY").map(PathBuf::from);
    let rolled_back =
        std::env::var_os("EMP_UPDATE_RESULT").is_some_and(|value| value == "rolled_back");
    // SAFETY: run_server is entered synchronously before it creates BackendState, runtimes,
    // or any application threads that could concurrently access the process environment.
    unsafe {
        std::env::remove_var("EMP_UPDATE_READY");
        std::env::remove_var("EMP_UPDATE_RESULT");
    }
    UpdateStartupMarkers {
        ready_path,
        rolled_back,
    }
}
