use super::{ServerHandle, UpdateStartupMarkers};
use crate::app::{BackendSources, BackendState, ServerState};
use crate::cli::is_loopback;
use crate::error::AppError;
use crate::http::auth::{BOOTSTRAP_LIFETIME_SECONDS, BootstrapToken, SessionStore};
use crate::server::transport::HttpListener;
use crate::services::connection_admission::{ConnectionAdmission, ConnectionAdmissionConfig};
use crate::services::observation::diagnostics::Diagnostics;
use crate::util::system_now;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_state::{WEB_SESSION_TOKEN_BYTES, load_or_create_web_session, web_session_path};
use std::net::IpAddr;
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
    external_key: bool,
    official_models: emp_state::official_registry::RegistrySource,
}

impl ServerHandle {
    #[cfg(test)]
    pub fn start_with_config(
        host: IpAddr,
        port: u16,
        config_path: &Path,
    ) -> Result<Self, AppError> {
        let codex_home = config_path.with_file_name("codex");
        Self::start_with_config_options(
            host,
            port,
            config_path,
            "missing-test-codex",
            codex_home.join("auth.json"),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_for_test(
        host: IpAddr,
        config_path: &Path,
        native_auth_path: PathBuf,
    ) -> Result<Self, AppError> {
        Self::start_with_test_options(
            host,
            config_path,
            native_auth_path,
            "missing-test-codex",
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_test_options(
        host: IpAddr,
        config_path: &Path,
        native_auth_path: PathBuf,
        codex_binary: &str,
        http_client_override: Option<emp_transport::HttpClient>,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options_inner(StartupContext {
            host,
            port: 0,
            config_path,
            codex_binary,
            native_auth_path,
            open_browser: false,
            markers: UpdateStartupMarkers::default(),
            http_client_override,
            admission: ConnectionAdmissionConfig::default(),
            enable_catalog_refresh_worker: false,
            external_key: false,
            official_models: emp_state::official_registry::bundled_snapshot,
        })
    }

    #[cfg(test)]
    pub(crate) fn start_with_config_options(
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
            enable_catalog_refresh_worker: false,
            external_key: false,
            official_models: emp_state::official_registry::bundled_snapshot,
        })
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
            enable_catalog_refresh_worker: true,
            external_key: crate::startup_recovery::external_key(),
            official_models: emp_state::official_registry::snapshot,
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
            external_key: false,
            official_models: emp_state::official_registry::bundled_snapshot,
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
            external_key: false,
            official_models: emp_state::official_registry::bundled_snapshot,
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
            external_key: false,
            official_models: emp_state::official_registry::bundled_snapshot,
        })
    }

    fn start_with_config_options_inner(context: StartupContext<'_>) -> Result<Self, AppError> {
        let budget = super::resources::prepare();
        let config_path = emp_state::config::resolve_user_path(context.config_path);
        let diagnostics = Arc::new(Diagnostics::new(
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
        diagnostics.journal.event(
            "info",
            "descriptor_budget",
            &serde_json::json!({
                "initial_limit":budget.before,"soft_limit":budget.soft,
                "raised":budget.raised,"os_error":budget.os_error,
            }),
        );
        let record_failure = |error: &AppError| {
            diagnostics.journal.event_and_flush_blocking(
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
        };
        let enable_catalog_refresh_worker = context.enable_catalog_refresh_worker;
        let (mut handle, listener) =
            Self::start_recorded(context, Arc::clone(&diagnostics)).inspect_err(record_failure)?;
        if let Err(error) = handle.start_workers(listener, enable_catalog_refresh_worker) {
            record_failure(&error);
            let _ = handle.shutdown();
            return Err(error);
        }
        Ok(handle)
    }

    fn start_recorded(
        context: StartupContext<'_>,
        diagnostics: Arc<Diagnostics>,
    ) -> Result<(Self, HttpListener), AppError> {
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
            enable_catalog_refresh_worker: _,
            external_key,
            official_models,
        } = context;
        let admission = super::resources::prepare().admission(admission);
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
        use crate::services::modules::Startup;
        let update = match &markers.ready_path {
            None => Startup::Normal,
            Some(marker) => {
                if emp_updater::worker::validate_startup_marker(crate::VERSION, marker)
                    .map_err(std::io::Error::other)?
                {
                    Startup::PreparedUpdate
                } else {
                    Startup::BundledUpdate
                }
            }
        };
        let backend = BackendState::new(
            config_path,
            codex_binary,
            native_auth_path,
            http_client_override,
            diagnostics,
            update,
            BackendSources {
                external_key,
                official_models,
            },
        )?;
        let (listener, local_addr) = HttpListener::bind(host, port)?;
        let sessions = Arc::new(SessionStore::new(session, session_path));
        let mut random = [0_u8; WEB_SESSION_TOKEN_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| AppError::RandomUnavailable)?;
        let lifecycle =
            Arc::new_cyclic(|control: &std::sync::Weak<crate::server::ServerControl>| {
                let control = control.clone();
                crate::server::ServerControl::new(Arc::new(ConnectionAdmission::new(admission)))
                    .with_backend(
                        crate::services::updates::UpdateState::new(
                            config_path,
                            crate::VERSION,
                            local_addr,
                            open_browser,
                            markers.rolled_back,
                            move || {
                                if let Some(control) = control.upgrade() {
                                    control.stop(crate::server::ShutdownReason::Requested);
                                }
                                Ok(())
                            },
                            Arc::clone(&backend.diagnostics),
                            Arc::clone(&backend.modules),
                        ),
                        Arc::clone(&backend.management_events),
                    )
            });
        let state = Arc::new(ServerState {
            test_sideband_address: Default::default(),
            lifecycle,
            catalog_refresh: crate::services::account_catalog::CatalogRefreshState::default(),
            sessions,
            bootstrap: BootstrapToken {
                token: URL_SAFE_NO_PAD.encode(random),
                used: AtomicBool::new(false),
                expires_at: system_now() + BOOTSTRAP_LIFETIME_SECONDS,
            },
            reopen_bootstrap: std::sync::Mutex::new(None),
            backend,
            port: local_addr.port(),
            base_url: crate::services::integration::endpoint::codex_base_url(local_addr),
        });
        let journal = &state.backend.diagnostics.journal;
        journal.event(
            "info",
            "proxy_selected",
            &serde_json::json!({
                "source": state.backend.transport.support_network.source_at_startup,
            }),
        );
        let handle = Self {
            local_addr,
            state,
            listener: None,
            workers: Vec::new(),
            _service_owner: service_owner,
        };
        Ok((handle, listener))
    }

    fn start_workers(
        &mut self,
        listener: HttpListener,
        enable_catalog_refresh_worker: bool,
    ) -> Result<(), AppError> {
        self.listener = Some(listener.start(self.state.clone())?);
        if enable_catalog_refresh_worker {
            self.spawn_worker("emp-catalog-refresh", |state| {
                crate::services::account_catalog::run_refresh_worker(&state);
            })?;
            crate::services::account_catalog::request_refresh(&self.state, false);
        }
        self.spawn_worker("emp-team-usage", |state| {
            crate::services::team::TeamState::run(&state);
        })?;
        self.spawn_worker("emp-quota-sampler", |state| {
            crate::services::quota::run_sampler(&state);
        })?;
        self.spawn_worker("emp-runtime-watch", |state| {
            crate::services::runtime::watch_runtime(&state);
        })?;
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        self.spawn_worker("emp-heap-reclaim", super::memory::run)?;
        self.state.backend.diagnostics.journal.event(
            "info",
            "service_listening",
            &serde_json::json!({"port": self.local_addr.port()}),
        );
        Ok(())
    }
}
