//! Server construction, background tasks and shutdown.

use crate::app::BackendState;
use crate::app::ServerState;
use crate::cli::is_loopback;
use crate::error::AppError;
use crate::http::auth::BootstrapToken;
use crate::http::auth::SessionStore;
use crate::http::auth::codex_auth_path;
#[cfg(test)]
use crate::http::auth::session_cookie;
use crate::http::routes::handle_connection;
use crate::services::quota::sample_quotas_once;
use crate::util::system_now;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_state::WEB_SESSION_TOKEN_BYTES;
use emp_state::WebSession;
use emp_state::config_path;
use emp_state::load_or_create_web_session;
use emp_state::web_session_path;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(5 * 60);

pub(crate) struct ServerHandle {
    local_addr: SocketAddr,
    pub(crate) state: Arc<ServerState>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ServerHandle {
    pub fn start(host: IpAddr, port: u16) -> Result<Self, AppError> {
        Self::start_with_config(host, port, &config_path())
    }

    pub fn start_with_config(
        host: IpAddr,
        port: u16,
        config_path: &Path,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options(host, port, config_path, "codex", codex_auth_path())
    }

    pub(crate) fn start_with_config_options(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
    ) -> Result<Self, AppError> {
        if !is_loopback(host) {
            return Err(AppError::HostNotLoopback);
        }
        let resolved_config = emp_state::config::resolve_user_path(config_path);
        let config_path = resolved_config.as_path();
        let service_owner = emp_state::IntegrationFileLock::acquire(
            &config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("state/service.lock"),
            Duration::ZERO,
            Duration::from_millis(20),
        )
        .map_err(|_| AppError::ServiceOwned)?;
        let now = system_now();
        let session_path = web_session_path(config_path)?;
        let session =
            load_or_create_web_session(&session_path, now).map_err(AppError::WebSession)?;
        let backend = BackendState::new(config_path, codex_binary, native_auth_path)?;
        Self::start_with_session(host, port, session_path, session, backend, service_owner)
    }

    fn start_with_session(
        host: IpAddr,
        port: u16,
        session_path: PathBuf,
        session: WebSession,
        backend: BackendState,
        service_owner: emp_state::IntegrationFileLock,
    ) -> Result<Self, AppError> {
        let listener = TcpListener::bind((host, port))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionStore {
            session: Mutex::new(session),
            path: session_path,
        });
        let mut random = [0_u8; WEB_SESSION_TOKEN_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| AppError::RandomUnavailable)?;
        let state = Arc::new(ServerState {
            shutdown,
            _service_owner: service_owner,
            sessions,
            bootstrap: BootstrapToken {
                token: URL_SAFE_NO_PAD.encode(random),
                used: AtomicBool::new(false),
            },
            backend,
            port: local_addr.port(),
        });
        let workers = Arc::new(Mutex::new(Vec::new()));
        let handle = Self {
            local_addr,
            state,
            workers,
        };
        handle.add_worker(listener)?;
        handle.add_quota_sampler()?;
        Ok(handle)
    }

    fn add_worker(&self, listener: TcpListener) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-http".to_string())
            .spawn(move || {
                loop {
                    if state.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let request_state = Arc::clone(&state);
                            let _ = thread::Builder::new()
                                .name("emp-request".to_string())
                                .spawn(move || handle_connection(stream, &request_state));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(AppError::Io)?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        Ok(())
    }

    fn add_quota_sampler(&self) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-quota-sampler".to_owned())
            .spawn(move || {
                while !state.shutdown.load(Ordering::Acquire) {
                    let deadline = Instant::now() + QUOTA_SAMPLE_INTERVAL;
                    let mut wait = match state.backend.accounts.quota_sampler_wait.lock() {
                        Ok(wait) => wait,
                        Err(_) => return,
                    };
                    loop {
                        if state.shutdown.load(Ordering::Acquire) {
                            return;
                        }
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let result = state
                            .backend
                            .accounts
                            .quota_sampler_condition
                            .wait_timeout(wait, deadline.saturating_duration_since(now));
                        match result {
                            Ok((next, _)) => wait = next,
                            Err(_) => return,
                        }
                    }
                    drop(wait);
                    if !state.shutdown.load(Ordering::Acquire) {
                        sample_quotas_once(&state);
                    }
                }
            })
            .map_err(AppError::Io)?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
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
    pub fn session_cookie(&self) -> String {
        let session = self
            .state
            .sessions
            .session
            .lock()
            .expect("session lock is not poisoned");
        session_cookie(session.token(), session.remaining_seconds_at(system_now()))
    }

    fn reconcile_startup(&self) {
        if let Ok(result) = self.state.backend.integration.manager.recover(true, true)
            && result.ok()
            && result.state == "active"
        {
            self.state
                .backend
                .integration
                .owned
                .store(true, Ordering::Release);
        }
    }

    pub fn shutdown(self) -> Result<(), AppError> {
        let restoration = self.state.backend.integration.restore_owned();
        self.state.shutdown.store(true, Ordering::Release);
        self.state.backend.accounts.quota_condition.notify_all();
        self.state
            .backend
            .accounts
            .quota_sampler_condition
            .notify_all();
        let workers = match Arc::try_unwrap(self.workers) {
            Ok(workers) => workers,
            Err(_) => return Err(AppError::ServerStopped),
        };
        let workers: Vec<JoinHandle<()>> =
            workers.into_inner().map_err(|_| AppError::ServerStopped)?;
        for worker in workers {
            let _: () = worker.join().map_err(|_| AppError::ServerStopped)?;
        }
        restoration
    }
}

pub(crate) fn run_server(config: Option<&Path>, host: IpAddr, port: u16) -> Result<(), AppError> {
    let server = match config {
        Some(config) => ServerHandle::start_with_config(host, port, config)?,
        None => ServerHandle::start(host, port)?,
    };
    server.reconcile_startup();
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
        println!("Shutdown: terminate the process (SIGINT/SIGTERM where supported)");
        println!("Open in browser: {}", server.bootstrap_url());
        let requested = async {
            while !server.state.shutdown.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
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
