//! Construct isolated state and register the service's background workers.

use super::{ServerHandle, UpdateStartupMarkers, connections};
use crate::app::{BackendState, ServerState};
use crate::cli::is_loopback;
use crate::error::AppError;
use crate::http::auth::{BOOTSTRAP_LIFETIME_SECONDS, BootstrapToken, SessionStore};
use crate::services::connection_admission::{ConnectionAdmission, ConnectionAdmissionConfig};
use crate::util::system_now;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_state::{WEB_SESSION_TOKEN_BYTES, load_or_create_web_session, web_session_path};
use std::net::{IpAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

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

impl ServerHandle {
    #[cfg(test)]
    pub fn start_with_config(
        host: IpAddr,
        port: u16,
        config_path: &Path,
    ) -> Result<Self, AppError> {
        // Test fixtures must not borrow the developer's native credentials,
        // integration lease or generated model catalog. Explicit integration
        // fixtures can still provide their own home through the options API.
        let codex_home = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("codex");
        let server = Self::start_with_config_options(
            host,
            port,
            config_path,
            "missing-test-codex",
            codex_home.join("auth.json"),
        )?;
        let defaults = emp_state::normalize_configuration(None)?;
        {
            let mut config = server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap();
            if config["native_catalog_path"] == defaults["native_catalog_path"] {
                config["native_catalog_path"] =
                    serde_json::json!(codex_home.join("models_cache.json"));
            }
        }
        Ok(server)
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

    pub(super) fn start_with_config_options_and_browser(
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
        let listener = TcpListener::bind((host, port))?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_wake = Arc::new(tokio::sync::Notify::new());
        let sessions = Arc::new(SessionStore::new(session, session_path));
        let mut random = [0_u8; WEB_SESSION_TOKEN_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| AppError::RandomUnavailable)?;
        let updates = crate::services::updates::UpdateState::new(
            config_path,
            crate::VERSION,
            local_addr,
            open_browser,
            markers.rolled_back,
            Arc::clone(&shutdown),
            Arc::clone(&shutdown_wake),
            Arc::clone(&backend.diagnostics),
        );
        let state = Arc::new(ServerState {
            shutdown,
            shutdown_wake,
            catalog_refresh: crate::services::account_catalog::CatalogRefreshState::default(),
            sessions,
            connection_admission: ConnectionAdmission::new(admission),
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
        let mut handle = Self {
            local_addr,
            state,
            workers: Vec::new(),
            _service_owner: service_owner,
        };
        let started = (|| {
            handle.spawn_worker("emp-http", move |state| {
                connections::accept(listener, state);
            })?;
            if enable_catalog_refresh_worker {
                handle.spawn_worker("emp-catalog-refresh", |state| {
                    crate::services::account_catalog::run_refresh_worker(&state);
                })?;
                crate::services::account_catalog::request_refresh(&handle.state, false);
            }
            handle.spawn_worker("emp-quota-sampler", |state| {
                crate::services::quota::run_sampler(&state);
            })?;
            handle.spawn_worker("emp-runtime-watch", |state| {
                crate::services::runtime::watch_runtime(&state);
            })?;
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
}
