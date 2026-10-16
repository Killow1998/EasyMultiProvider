//! Service ownership, background-worker registration and ordered shutdown.

mod connections;
mod shutdown;
mod signals;
mod startup;
#[cfg(windows)]
mod windows;

use crate::app::ServerState;
use crate::error::AppError;
use crate::http::auth::codex_auth_path;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

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
    let signals = {
        let _entered = server.state.backend.transport.runtime.enter();
        signals::ExitSignals::new(Arc::clone(&server.state))
    };
    let signals = match signals {
        Ok(signals) => signals,
        Err(error) => {
            let _ = server.shutdown();
            return Err(error.into());
        }
    };
    let mut restore = shutdown::RestoreBeforeExit::new(Arc::clone(&server.state));
    let prepared = (|| {
        server
            .state
            .backend
            .configuration
            .set_listener(host, server.local_addr().port())
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
        restore.disarm();
        return Err(error);
    }
    let result = server.state.backend.transport.runtime.block_on(async {
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
        // Update and rollback keep the existing browser session and tab.
        let initial_launch = markers.ready_path.is_none() && !markers.rolled_back;
        if open_browser && initial_launch && !crate::cli::desktop::open_browser(&bootstrap_url) {
            println!("Browser did not open automatically; use the URL above.");
        }
        emp_state::update::worker::mark_ready(crate::VERSION, markers.ready_path.clone())
            .map_err(std::io::Error::other)?;
        server.state.wait_for_shutdown().await;
        Ok::<bool, std::io::Error>(signals.external_exit())
    });
    // Only the updater's own handoff retains integration. Closing a terminal
    // during an update still restores native routing before this process exits.
    let handoff =
        matches!(result, Ok(false)) && server.state.updates.snapshot().state == "installing";
    if handoff {
        restore.disarm();
    }
    let cleanup = server.shutdown_with_handoff(handoff);
    restore.disarm();
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
