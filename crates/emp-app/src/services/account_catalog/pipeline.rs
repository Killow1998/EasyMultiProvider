use crate::app::ServerState;
use crate::services::accounts::{
    account_catalog_headers, duplicate_accounts, native_auth_document,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub(crate) struct RefreshSource {
    pub(crate) id: String,
    pub(crate) path: PathBuf,
    pub(crate) base: String,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) owner: String,
    credential_fingerprint: String,
    pub(crate) client_version: String,
    pub(crate) generation: u64,
    pub(crate) native: bool,
    pub(crate) worker_eligible: bool,
}

impl RefreshSource {
    pub(crate) fn native(
        state: &ServerState,
        config: &Value,
        base: &str,
        client_version: String,
    ) -> Option<Self> {
        let headers = native_auth_document(&state.backend.accounts.native_auth_path)
            .and_then(|auth| emp_codex::account_auth_headers(&auth))?;
        let owner = emp_codex::native_catalog_owner(&headers);
        let credential_fingerprint = credential_fingerprint(&headers);
        let path = emp_codex::native_catalog_path(config.as_object()?);
        Some(Self {
            id: "@native".to_owned(),
            path,
            base: base.to_owned(),
            headers,
            owner,
            credential_fingerprint,
            client_version,
            generation: 0,
            native: true,
            worker_eligible: false,
        })
    }

    pub(crate) fn account(
        state: &ServerState,
        config: &Value,
        account: &Value,
        base: &str,
        client_version: String,
        generation: u64,
        worker_eligible: bool,
    ) -> Option<Self> {
        let id = account.get("id")?.as_str()?;
        let path = managed_account_catalog_path(state, config, account)?;
        let headers = account.as_object().and_then(|account| {
            account_catalog_headers(account, &state.backend.configuration.vault)
        })?;
        let owner = emp_codex::native_catalog_owner(&headers);
        let credential_fingerprint = credential_fingerprint(&headers);
        Some(Self {
            id: id.to_owned(),
            path,
            base: base.to_owned(),
            headers,
            owner,
            credential_fingerprint,
            client_version,
            generation,
            native: false,
            worker_eligible,
        })
    }

    pub(crate) fn fingerprint(&self) -> String {
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            self.base,
            self.path.display(),
            self.owner,
            self.credential_fingerprint,
            self.client_version,
            self.native
        )
    }
}

fn credential_fingerprint(headers: &BTreeMap<String, String>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"emp-catalog-credential-v1\0");
    for (name, value) in headers {
        digest.update(name.to_ascii_lowercase().as_bytes());
        digest.update([0]);
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    format!("sha256:{:x}", digest.finalize())
}

pub(crate) struct PersistedCatalog {
    pub(crate) catalog: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistError {
    RuntimeChanged,
    SourceChanged,
    Internal,
}

pub(crate) enum FetchAndPersistOutcome {
    Persisted(PersistedCatalog),
    UpstreamError(emp_router::RouterError),
    PersistenceError(PersistError),
    Stopped,
}

pub(crate) async fn fetch_catalog(
    client: &emp_transport::HttpClient,
    source: &RefreshSource,
) -> Result<Value, emp_router::RouterError> {
    emp_router::subscription_catalog::fetch(
        client,
        &source.base,
        &source.headers,
        &source.client_version,
    )
    .await
}

pub(crate) fn fetch_and_persist(
    state: &ServerState,
    source: &RefreshSource,
) -> FetchAndPersistOutcome {
    let request = fetch_catalog(&state.backend.transport.client, source);
    let shutdown = Arc::clone(&state.shutdown);
    let fetched = state.backend.transport.runtime.block_on(async move {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(shutdown) => None,
            result = request => Some(result),
        }
    });
    let Some(fetched) = fetched else {
        return FetchAndPersistOutcome::Stopped;
    };
    let catalog = match fetched {
        Ok(catalog) => catalog,
        Err(error) => return FetchAndPersistOutcome::UpstreamError(error),
    };
    if state.shutdown.load(Ordering::Acquire) {
        return FetchAndPersistOutcome::Stopped;
    }
    match persist_fetched_catalog(state, source, catalog) {
        Ok(persisted) => FetchAndPersistOutcome::Persisted(persisted),
        Err(error) => FetchAndPersistOutcome::PersistenceError(error),
    }
}

async fn wait_for_shutdown(shutdown: Arc<std::sync::atomic::AtomicBool>) {
    while !shutdown.load(Ordering::Acquire) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

pub(crate) fn persist_fetched_catalog(
    state: &ServerState,
    source: &RefreshSource,
    mut catalog: Value,
) -> Result<PersistedCatalog, PersistError> {
    let current_version = state
        .backend
        .integration
        .inventory
        .selected_trusted_version()
        .unwrap_or_else(emp_codex::runtime_inventory::minimum_codex);
    if current_version != source.client_version {
        return Err(PersistError::RuntimeChanged);
    }

    let current = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| PersistError::Internal)?;
    if current.get("codex_base_url").and_then(Value::as_str) != Some(&source.base) {
        return Err(PersistError::SourceChanged);
    }
    let path = if source.native {
        let current_headers = native_auth_document(&state.backend.accounts.native_auth_path)
            .and_then(|auth| emp_codex::account_auth_headers(&auth));
        let Some(current_map) = current.as_object() else {
            return Err(PersistError::SourceChanged);
        };
        if emp_codex::native_catalog_path(current_map) != source.path
            || current_headers.as_ref() != Some(&source.headers)
        {
            return Err(PersistError::SourceChanged);
        }
        source.path.clone()
    } else {
        let Some(accounts) = current.get("accounts").and_then(Value::as_array) else {
            return Err(PersistError::SourceChanged);
        };
        let matching = accounts
            .iter()
            .filter(|account| account.get("id").and_then(Value::as_str) == Some(&source.id))
            .collect::<Vec<_>>();
        if source.worker_eligible
            && (matching.len() != 1 || matching[0]["enabled"] == Value::Bool(false))
        {
            return Err(PersistError::SourceChanged);
        }
        let Some(account) = matching.first().copied() else {
            return Err(PersistError::SourceChanged);
        };
        let current_path = managed_account_catalog_path(state, &current, account);
        let current_headers = account.as_object().and_then(|account| {
            account_catalog_headers(account, &state.backend.configuration.vault)
        });
        let current_owner = current_headers
            .as_ref()
            .map(emp_codex::native_catalog_owner);
        if current_path.as_ref() != Some(&source.path)
            || current_owner.as_deref() != Some(&source.owner)
            || current_headers
                .as_ref()
                .is_none_or(|headers| headers.is_empty())
            || state.catalog_refresh.account_generation(&source.id) != source.generation
        {
            return Err(PersistError::SourceChanged);
        }
        if source.worker_eligible {
            let duplicates = duplicate_accounts(
                &current,
                &state.backend.configuration.vault,
                &state.backend.accounts.native_auth_path,
            );
            if duplicates.contains_key(&source.id) {
                return Err(PersistError::SourceChanged);
            }
        }
        catalog["account_owner"] = json!(source.owner);
        catalog["base_url"] = json!(source.base);
        source.path.clone()
    };

    let version_after_lock = state
        .backend
        .integration
        .inventory
        .selected_trusted_version()
        .unwrap_or_else(emp_codex::runtime_inventory::minimum_codex);
    if version_after_lock != source.client_version {
        return Err(PersistError::RuntimeChanged);
    }

    let previous = if source.native {
        emp_codex::load_native_catalog(&current)
    } else {
        super::read_account_cache(&path).unwrap_or_else(|| json!({}))
    };
    let changed = previous != catalog;
    if changed {
        state.catalog_refresh.mark_catalog_publication_pending();
        emp_state::filesystem::write_catalog_json(&path, &catalog)
            .map_err(|_| PersistError::Internal)?;
    }
    if source.native && emp_codex::preserve_native_catalog(&current).is_err() {
        return Err(PersistError::Internal);
    }
    Ok(PersistedCatalog { catalog })
}

pub(crate) fn managed_account_catalog_path(
    state: &ServerState,
    config: &Value,
    account: &Value,
) -> Option<PathBuf> {
    let id = account.get("id")?.as_str()?;
    let configured_auth = account.get("auth_file")?.as_str()?;
    if configured_auth.is_empty() {
        return None;
    }
    let expected =
        emp_state::account_auth_path(config, id, &state.backend.configuration.config_path).ok()?;
    let configured = emp_state::config::resolve_user_path(&PathBuf::from(configured_auth));
    if configured != expected {
        return None;
    }
    Some(expected.parent()?.join("models_cache.json"))
}

pub(crate) fn publish_pending_catalog(state: &ServerState) -> Result<(), ()> {
    let path = crate::services::catalog::generated_catalog_path(state);
    if !state.catalog_refresh.catalog_publication_pending() && path.is_file() {
        return Ok(());
    }
    state.catalog_refresh.mark_catalog_publication_pending();
    crate::services::catalog::refresh_catalog(state).map(|_| ())
}
