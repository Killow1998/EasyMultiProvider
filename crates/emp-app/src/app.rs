use crate::error::AppError;
use crate::services::accounts::AccountState;
use crate::services::activity::ActivityService;
use crate::services::configuration::{ConfigurationState, load_server_configuration};
use crate::services::integration::IntegrationState;
use crate::services::observation::diagnostics::Diagnostics;
use crate::services::quota::QuotaState;
use crate::util::{random_hex, system_now};
use emp_state::config::save_configuration_with_registry;
use emp_state::official_registry::RegistrySource;
use emp_transport::{
    HttpClientPolicy, ProxyEnvironment, ProxyPolicy, RequestLimitsConfig, TimeoutPolicy,
};
use std::path::Path;
use tokio::runtime::Builder as RuntimeBuilder;

use crate::http::auth::BootstrapToken;
use crate::http::auth::SessionStore;
use emp_codex::configuration::IntegrationManager;
use emp_transport::HttpClient;
use emp_transport::RequestLimits;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tokio::runtime::Runtime;

pub(crate) struct ServerState {
    pub(crate) lifecycle: Arc<crate::server::ServerControl>,
    pub(crate) catalog_refresh: crate::services::account_catalog::CatalogRefreshState,
    pub(crate) sessions: Arc<SessionStore>,
    pub(crate) bootstrap: BootstrapToken,
    pub(crate) reopen_bootstrap: Mutex<Option<BootstrapToken>>,
    pub(crate) backend: BackendState,
    pub(crate) port: u16,
    pub(crate) base_url: String,
    pub(crate) test_sideband_address: crate::api::realtime::sideband::SidebandProbeAddress,
}

impl ServerState {
    pub(crate) fn reopen_url(&self) -> Result<String, AppError> {
        let token = BootstrapToken {
            token: crate::util::random_hex(emp_state::WEB_SESSION_TOKEN_BYTES)?,
            used: AtomicBool::new(false),
            expires_at: crate::util::system_now() + crate::http::auth::BOOTSTRAP_LIFETIME_SECONDS,
        };
        let url = format!("http://127.0.0.1:{}/?bootstrap={}", self.port, token.token);
        *self
            .reopen_bootstrap
            .lock()
            .map_err(|_| AppError::ServerStopped)? = Some(token);
        Ok(url)
    }

    pub(crate) fn request_shutdown(&self) {
        self.lifecycle
            .stop(crate::server::ShutdownReason::Requested);
    }

    pub(crate) async fn wait_for_shutdown(&self) {
        self.lifecycle.signal().wait().await;
    }
}

pub(crate) struct BackendState {
    pub(crate) history_summaries: emp_context::context::SummaryCache,
    pub(crate) modules: Arc<crate::services::modules::Modules>,
    pub(crate) team: crate::services::team::TeamState,
    pub(crate) aero_callers: crate::services::modules::aero::Callers,
    pub(crate) gateway_key: String,
    pub(crate) gateway_clients: crate::services::gateway_clients::GatewayClients,
    pub(crate) claude_turns: crate::services::claude_cli::tui::TurnPool,
    pub(crate) configuration: ConfigurationState,
    pub(crate) transport: TransportState,
    pub(crate) native_websocket_connections: crate::services::native_connections::NativeConnections,
    pub(crate) codex: CodexPaths,
    pub(crate) accounts: AccountState,
    pub(crate) quota: QuotaState,
    pub(crate) activity: ActivityService,
    pub(crate) claude_quota: crate::services::claude_cli::quota::LocalQuota,
    pub(crate) provider_balances: crate::services::provider_balance::Balances,
    pub(crate) provider_quotas: crate::services::cpa_quota::PoolQuotas,
    pub(crate) app_icon: crate::services::app_icon::AppIcon,
    pub(crate) auto_review: crate::services::auto_review::ReviewState,
    pub(crate) aggregate_availability: crate::services::auto_review::ReviewState,
    pub(crate) memory_status: crate::services::memory_routing::MemoryStatus,
    pub(crate) management_events: Arc<crate::services::management_events::ManagementEvents>,
    pub(crate) integration: IntegrationState,
    pub(crate) usage: crate::services::usage::UsageState,
    pub(crate) diagnostics: Arc<Diagnostics>,
    pub(crate) migration_previews: crate::services::migration::MigrationPreviews,
    pub(crate) native_login: crate::services::native_login::LoginSession,
    pub(crate) account_login: crate::services::native_login::LoginSession,
    pub(crate) lan_migration: crate::services::migration::lan::LanMigration,
}

pub(crate) struct CodexPaths {
    pub(crate) auth_path: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) binary: String,
}

pub(crate) struct TransportState {
    pub(crate) client: Arc<HttpClient>,
    pub(crate) support_network: crate::services::network_evidence::NetworkSnapshot,
    pub(crate) runtime: Runtime,
    pub(crate) request_limits: Arc<RequestLimits>,
    pub(crate) image_routes: crate::services::native::images::ImageRoutes,
    pub(crate) gemini_signatures: Arc<emp_state::gemini_signatures::GeminiSignatures>,
}

pub(crate) struct BackendSources {
    pub(crate) external_key: bool,
    pub(crate) official_models: RegistrySource,
}

impl BackendState {
    pub(crate) fn new(
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
        http_client_override: Option<HttpClient>,
        diagnostics: Arc<Diagnostics>,
        updating: crate::services::modules::Startup,
        sources: BackendSources,
    ) -> Result<Self, AppError> {
        crate::startup_recovery::check_pending(config_path)?;
        let mut config = load_server_configuration(config_path, sources.official_models)?;
        let state_root = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state");
        let vault =
            crate::startup_recovery::open_vault(&config, &state_root, sources.external_key)?;
        if config
            .get("providers")
            .and_then(Value::as_array)
            .is_some_and(|providers| {
                providers.iter().any(|provider| {
                    provider
                        .get("api_key")
                        .and_then(Value::as_str)
                        .is_some_and(|key| !key.is_empty())
                })
            })
        {
            save_configuration_with_registry(
                &config,
                Some(config_path),
                &vault,
                sources.official_models,
            )?;
            config = load_server_configuration(config_path, sources.official_models)?;
        }
        let proxy_snapshot = ProxyEnvironment::capture_current();
        let support_network = crate::services::network_evidence::NetworkSnapshot::capture(
            &proxy_snapshot.environment,
            proxy_snapshot.source,
        );
        let client = match http_client_override {
            Some(client) => client,
            None => HttpClient::new(HttpClientPolicy::new(
                ProxyPolicy::dynamic_environment(),
                TimeoutPolicy::default(),
            ))?,
        };
        let runtime = RuntimeBuilder::new_multi_thread()
            .enable_all()
            .thread_name("emp-upstream")
            .build()?;
        let request_limits = RequestLimits::new(
            RequestLimitsConfig::default(),
            emp_transport::system_memory_status,
            || system_now().max(0.0) as u64,
            random_hex(16)?,
        )?;
        let codex_home = native_auth_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let integration = IntegrationManager::new(
            codex_home.join("config.toml"),
            codex_home
                .join("easy-multi-provider")
                .join("integration")
                .join("lease.json"),
            None,
        )
        .map_err(|_| AppError::ServerStopped)?
        .with_lock_path(codex_home.join("easy-multi-provider/integration/lease.lock"));
        let management_events =
            Arc::new(crate::services::management_events::ManagementEvents::default());
        let gateway_path = state_root.join("gateway-clients.json");
        let gateway_clients =
            crate::services::gateway_clients::GatewayClients::open(gateway_path.clone(), &vault);
        let modules = crate::services::modules::Modules::open(
            state_root.join("modules"),
            updating,
            &config,
            gateway_clients
                .as_ref()
                .is_ok_and(|clients| clients.has_devices()),
        )
        .map_err(std::io::Error::other)?;
        let gateway_clients = match gateway_clients {
            Ok(clients) => clients,
            Err(AppError::Filesystem(emp_state::FilesystemError::InvalidJson)) => {
                modules.mark_unavailable(emp_modules::Module::Gateway, "invalid_gateway_clients");
                crate::services::gateway_clients::GatewayClients::empty(gateway_path)
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            history_summaries: Default::default(),
            modules: Arc::new(modules),
            aero_callers: Default::default(),
            team: crate::services::team::TeamState::open(
                state_root.join("team-profile.json"),
                &vault,
                &config,
            ),
            gateway_key: emp_state::gateway_key::load_or_create(&state_root.join("gateway-key"))?,
            gateway_clients,
            claude_turns: Default::default(),
            usage: crate::services::usage::UsageState::new(
                &state_root,
                Arc::clone(&management_events),
            ),
            diagnostics,
            configuration: ConfigurationState::new(
                config,
                config_path.to_path_buf(),
                vault,
                sources.official_models,
            ),
            native_websocket_connections: Default::default(),
            transport: TransportState {
                client: Arc::new(client),
                support_network,
                runtime,
                request_limits,
                image_routes: Default::default(),
                gemini_signatures: Arc::new(emp_state::gemini_signatures::GeminiSignatures::new(
                    state_root.join("gemini-signatures.sqlite3"),
                )),
            },
            codex: CodexPaths {
                auth_path: native_auth_path,
                home: codex_home.clone(),
                binary: codex_binary.to_owned(),
            },
            accounts: AccountState::default(),
            quota: QuotaState::new(&state_root),
            activity: ActivityService::new(Arc::clone(&management_events)),
            auto_review: Default::default(),
            aggregate_availability: Default::default(),
            provider_balances: Default::default(),
            provider_quotas: Default::default(),
            app_icon: crate::services::app_icon::AppIcon::new(&state_root),
            memory_status: Default::default(),
            migration_previews: Default::default(),
            native_login: Default::default(),
            account_login: Default::default(),
            lan_migration: Default::default(),
            claude_quota: crate::services::claude_cli::quota::LocalQuota::new(
                state_root.join("claude_quota_history.sqlite3"),
            ),
            management_events,
            integration: IntegrationState::new(integration, codex_home, codex_binary),
        })
    }
}
