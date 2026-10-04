//! Server construction, background tasks and shutdown.

use crate::app::BackendState;
use crate::app::ServerState;
use crate::cli::is_loopback;
use crate::error::AppError;
use crate::http::auth::BOOTSTRAP_LIFETIME_SECONDS;
use crate::http::auth::BootstrapToken;
use crate::http::auth::SessionStore;
use crate::http::auth::codex_auth_path;
use crate::http::routes::handle_connection;
use crate::services::connection_admission::{ConnectionAdmission, ConnectionAdmissionConfig};
use crate::services::quota::sample_quotas_once;
use crate::util::system_now;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_state::WEB_SESSION_TOKEN_BYTES;
use emp_state::WebSession;
use emp_state::load_or_create_web_session;
use emp_state::web_session_path;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
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

#[cfg(not(test))]
const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(44);
/// Unit tests drive `sample_quotas_once` themselves; keep the background sampler out of the way.
#[cfg(test)]
const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Longest a quota check can still rotate a credential: two 45 s Codex
/// queries plus the save retries.
const CREDENTIAL_DRAIN_TIMEOUT: Duration = Duration::from_secs(100);

pub(crate) struct ServerHandle {
    local_addr: SocketAddr,
    pub(crate) state: Arc<ServerState>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    _service_owner: emp_state::IntegrationFileLock,
}

struct ServerStartupOptions {
    config_path: PathBuf,
    open_browser: bool,
    markers: UpdateStartupMarkers,
    admission: ConnectionAdmissionConfig,
    enable_catalog_refresh_worker: bool,
}

struct StartupContext<'a> {
    host: IpAddr,
    port: u16,
    config_path: &'a Path,
    codex_binary: &'a str,
    native_auth_path: PathBuf,
    open_browser: bool,
    markers: UpdateStartupMarkers,
    http_client_override: Option<emp_transport::HttpClient>,
    admission: ConnectionAdmissionConfig,
    enable_catalog_refresh_worker: bool,
}

#[derive(Clone, Default)]
struct UpdateStartupMarkers {
    ready_path: Option<PathBuf>,
    rolled_back: bool,
}

impl ServerHandle {
    #[cfg(test)]
    pub fn start_with_config(
        host: IpAddr,
        port: u16,
        config_path: &Path,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options(host, port, config_path, "codex", codex_auth_path())
    }

    #[cfg(test)]
    pub(crate) fn start_with_config_options(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options_and_browser(
            host,
            port,
            config_path,
            codex_binary,
            native_auth_path,
            false,
            UpdateStartupMarkers::default(),
        )
    }

    fn start_with_config_options_and_browser(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
        open_browser: bool,
        markers: UpdateStartupMarkers,
    ) -> Result<Self, AppError> {
        // Unit-test servers do not start a network worker by default; the
        // catalog loopback integration fixture opts in explicitly below.
        Self::start_with_config_options_inner(StartupContext {
            host,
            port,
            config_path,
            codex_binary,
            native_auth_path,
            open_browser,
            markers,
            http_client_override: None,
            admission: ConnectionAdmissionConfig::default(),
            enable_catalog_refresh_worker: !cfg!(test),
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn start_with_catalog_refresh_for_test(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options_inner(StartupContext {
            host,
            port,
            config_path,
            codex_binary,
            native_auth_path,
            open_browser: false,
            markers: UpdateStartupMarkers::default(),
            http_client_override: None,
            admission: ConnectionAdmissionConfig::default(),
            enable_catalog_refresh_worker: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn start_with_config_options_and_http_client(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
        client: emp_transport::HttpClient,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options_inner(StartupContext {
            host,
            port,
            config_path,
            codex_binary,
            native_auth_path,
            open_browser: false,
            markers: UpdateStartupMarkers::default(),
            http_client_override: Some(client),
            admission: ConnectionAdmissionConfig::default(),
            enable_catalog_refresh_worker: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn start_with_connection_admission_for_test(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        admission: ConnectionAdmissionConfig,
    ) -> Result<Self, AppError> {
        let native_auth_path = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("codex/auth.json");
        Self::start_with_config_options_inner(StartupContext {
            host,
            port,
            config_path,
            codex_binary: "missing-test-codex",
            native_auth_path,
            open_browser: false,
            markers: UpdateStartupMarkers::default(),
            http_client_override: None,
            admission,
            enable_catalog_refresh_worker: false,
        })
    }

    fn start_with_config_options_inner(context: StartupContext<'_>) -> Result<Self, AppError> {
        let config_path = emp_state::config::resolve_user_path(context.config_path);
        let diagnostics = Arc::new(emp_state::diagnostics::Diagnostics::new(
            &config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("state"),
        ));
        diagnostics.journal.event(
            "info",
            "process_start",
            &serde_json::json!({
                "version":crate::VERSION, "platform":std::env::consts::OS, "pid":std::process::id(),
            }),
        );
        let result = Self::start_recorded(context, Arc::clone(&diagnostics));
        if let Err(error) = &result {
            diagnostics.journal.event(
                "warning",
                "startup_failure",
                &serde_json::json!({
                    "error_class":match error {
                        AppError::HostNotLoopback => "host_not_loopback",
                        AppError::ServiceOwned => "service_owned",
                        AppError::WebSession(_) => "web_session_error",
                        AppError::Config(_) => "config_error",
                        AppError::Filesystem(_) => "filesystem_error",
                        AppError::Io(_) => "io_error",
                        AppError::Transport(_) => "transport_error",
                        AppError::RequestLimits(_) => "request_limits_error",
                        AppError::RandomUnavailable => "random_unavailable",
                        _ => "startup_error",
                    },
                }),
            );
        }
        result
    }

    fn start_recorded(
        context: StartupContext<'_>,
        diagnostics: Arc<emp_state::diagnostics::Diagnostics>,
    ) -> Result<Self, AppError> {
        let StartupContext {
            host,
            port,
            config_path,
            codex_binary,
            native_auth_path,
            open_browser,
            markers,
            http_client_override,
            admission,
            enable_catalog_refresh_worker,
        } = context;
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
        let backend = BackendState::new(
            config_path,
            codex_binary,
            native_auth_path,
            http_client_override,
            diagnostics,
        )?;
        Self::start_with_session(
            host,
            port,
            session_path,
            session,
            backend,
            service_owner,
            ServerStartupOptions {
                config_path: config_path.to_path_buf(),
                open_browser,
                markers,
                admission,
                enable_catalog_refresh_worker,
            },
        )
    }

    fn start_with_session(
        host: IpAddr,
        port: u16,
        session_path: PathBuf,
        session: WebSession,
        backend: BackendState,
        service_owner: emp_state::IntegrationFileLock,
        startup_options: ServerStartupOptions,
    ) -> Result<Self, AppError> {
        let listener = TcpListener::bind((host, port))?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_wake = Arc::new(tokio::sync::Notify::new());
        let sessions = Arc::new(SessionStore::new(session, session_path));
        let mut random = [0_u8; WEB_SESSION_TOKEN_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| AppError::RandomUnavailable)?;
        let updates = crate::services::updates::UpdateState::new(
            &startup_options.config_path,
            crate::VERSION,
            local_addr,
            startup_options.open_browser,
            startup_options.markers.rolled_back,
            Arc::clone(&shutdown),
            Arc::clone(&shutdown_wake),
        );
        let state = Arc::new(ServerState {
            shutdown,
            shutdown_wake,
            catalog_refresh: crate::services::account_catalog::CatalogRefreshState::default(),
            sessions,
            connection_admission: ConnectionAdmission::new(startup_options.admission),
            bootstrap: BootstrapToken {
                token: URL_SAFE_NO_PAD.encode(random),
                used: AtomicBool::new(false),
                expires_at: system_now() + BOOTSTRAP_LIFETIME_SECONDS,
            },
            backend,
            port: local_addr.port(),
            base_url: format!("http://{local_addr}/v1"),
            updates,
        });
        let journal = &state.backend.diagnostics.journal;
        journal.event(
            "info",
            "proxy_selected",
            &serde_json::json!({
                "source": state.backend.transport.support_network.source_at_startup,
            }),
        );
        let workers = Arc::new(Mutex::new(Vec::new()));
        let handle = Self {
            local_addr,
            state,
            workers,
            _service_owner: service_owner,
        };
        let started = (|| {
            handle.add_worker(listener)?;
            if startup_options.enable_catalog_refresh_worker {
                handle.add_catalog_refresh_worker()?;
            }
            handle.add_quota_sampler()?;
            handle.add_runtime_watch()?;
            Ok::<(), AppError>(())
        })();
        if let Err(error) = started {
            let _ = handle.shutdown();
            return Err(error);
        }
        handle.state.backend.diagnostics.journal.event(
            "info",
            "service_listening",
            &serde_json::json!({"port": local_addr.port()}),
        );
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
                            if state.shutdown.load(Ordering::Acquire) {
                                drop(stream);
                                break;
                            }
                            let request_state = Arc::clone(&state);
                            let Some(request_permit) =
                                request_state.connection_admission.acquire_request()
                            else {
                                request_state.backend.diagnostics.journal.event("warning", "request_rejected", &serde_json::json!({"transport":"http", "reason":"connection_capacity"}));
                                drop(stream);
                                continue;
                            };
                            let diagnostics = Arc::clone(&state.backend.diagnostics);
                            if thread::Builder::new()
                                .name("emp-request".to_string())
                                .spawn(move || {
                                    handle_connection(stream, &request_state, Some(request_permit));
                                }).is_err() {
                                diagnostics.journal.event("warning", "request_rejected", &serde_json::json!({"transport":"http", "reason":"worker_spawn_failed"}));
                            }
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

    fn add_runtime_watch(&self) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-runtime-watch".to_owned())
            .spawn(move || crate::services::runtime::watch_runtime(&state))
            .map_err(AppError::Io)?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        Ok(())
    }

    fn add_catalog_refresh_worker(&self) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-catalog-refresh".to_owned())
            .spawn(move || crate::services::account_catalog::run_refresh_worker(&state))
            .map_err(AppError::Io)?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        crate::services::account_catalog::request_refresh(&self.state, false);
        Ok(())
    }

    fn add_quota_sampler(&self) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-quota-sampler".to_owned())
            .spawn(move || {
                crate::services::quota::migrate_legacy_quota_history(&state);
                // Sample right away so quota is ready when the page first opens.
                let mut deadline = if cfg!(test) {
                    Instant::now() + QUOTA_SAMPLE_INTERVAL
                } else {
                    Instant::now()
                };
                while !state.shutdown.load(Ordering::Acquire) {
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
                    deadline = Instant::now() + QUOTA_SAMPLE_INTERVAL;
                    if !state.shutdown.load(Ordering::Acquire) {
                        sample_quotas_once(&state);
                        // Disabled or duplicate accounts are not sampled;
                        // their rotated credentials still need saving.
                        crate::services::quota::flush_pending_rotations(&state);
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
        let workers = match Arc::try_unwrap(self.workers) {
            Ok(workers) => workers,
            Err(_) => return Err(AppError::ServerStopped),
        };
        let workers: Vec<JoinHandle<()>> =
            workers.into_inner().map_err(|_| AppError::ServerStopped)?;
        for worker in workers {
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
    let server = ServerHandle::start_with_config_options_and_browser(
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
        server
            .workers
            .lock()
            .map_err(|_| AppError::ServerStopped)?
            .extend(usage_workers);
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
