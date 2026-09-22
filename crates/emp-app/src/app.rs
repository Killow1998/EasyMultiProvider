//! Shared application service ownership.
use crate::error::AppError;
use crate::services::accounts::AccountState;
use crate::services::accounts::duplicate_accounts;
use crate::services::integration::IntegrationState;
use crate::services::providers::ConfigurationState;
use crate::util::{random_hex, system_now};
use emp_state::{load_configuration, save_configuration};
use emp_transport::{
    HttpClientPolicy, ProxyEnvironment, ProxyPolicy, RequestLimitsConfig, TimeoutPolicy,
};
use std::path::Path;
use tokio::runtime::Builder as RuntimeBuilder;

use crate::http::auth::BootstrapToken;
use crate::http::auth::SessionStore;
use emp_codex::quota_history::QuotaHistoryStore;
use emp_integration::IntegrationManager;
use emp_state::VaultStore;
use emp_transport::HttpClient;
use emp_transport::RequestLimits;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use tokio::runtime::Runtime;

pub(crate) struct ServerState {
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) _service_owner: emp_state::IntegrationFileLock,
    pub(crate) sessions: Arc<SessionStore>,
    pub(crate) bootstrap: BootstrapToken,
    pub(crate) backend: BackendState,
    pub(crate) port: u16,
}

pub(crate) struct BackendState {
    pub(crate) configuration: ConfigurationState,
    pub(crate) transport: TransportState,
    pub(crate) accounts: AccountState,
    pub(crate) integration: IntegrationState,
}

pub(crate) struct TransportState {
    pub(crate) client: HttpClient,
    pub(crate) runtime: Runtime,
    pub(crate) request_limits: Arc<RequestLimits>,
}

impl BackendState {
    pub(crate) fn new(
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
    ) -> Result<Self, AppError> {
        let mut config = load_configuration(Some(config_path))?;
        let state_root = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state");
        let vault = VaultStore::from_environment(&state_root.join("master.key"))?;
        let duplicates = duplicate_accounts(&config, &vault, &native_auth_path);
        let (migrated, changed) =
            emp_state::migrate_duplicate_native_visibility(&config, &duplicates);
        config = migrated;
        if changed
            || config
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
            save_configuration(&config, Some(config_path), &vault)?;
            config = load_configuration(Some(config_path))?;
        }
        let client = HttpClient::new(HttpClientPolicy::new(
            ProxyPolicy::from_environment(ProxyEnvironment::capture()),
            TimeoutPolicy::default(),
        ))?;
        let runtime = RuntimeBuilder::new_multi_thread()
            .enable_all()
            .thread_name("emp-upstream")
            .build()?;
        let request_limits = RequestLimits::new(
            RequestLimitsConfig::default(),
            || None,
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
        Ok(Self {
            configuration: ConfigurationState {
                config: Mutex::new(config),
                discovery_lock: Mutex::new(()),
                config_path: config_path.to_path_buf(),
                vault,
            },
            transport: TransportState {
                client,
                runtime,
                request_limits,
            },
            accounts: AccountState {
                native_auth_path,
                codex_home,
                codex_binary: codex_binary.to_owned(),
                native_quota: Mutex::new(None),
                quota_refresh_errors: Mutex::new(BTreeMap::new()),
                quota_refresh_locks: Mutex::new(BTreeMap::new()),
                quota_history: QuotaHistoryStore::new(state_root.join("quota_history.sqlite3")),
                quota_revision: Mutex::new(0),
                quota_condition: Condvar::new(),
                quota_event_slots: AtomicUsize::new(0),
                quota_sampler_wait: Mutex::new(()),
                quota_sampler_condition: Condvar::new(),
            },
            integration: IntegrationState {
                manager: integration,
                owned: AtomicBool::new(false),
            },
        })
    }
}
