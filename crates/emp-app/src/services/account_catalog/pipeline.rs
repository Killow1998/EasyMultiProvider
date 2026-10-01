use crate::app::ServerState;
use crate::services::accounts::{
    account_catalog_headers, duplicate_accounts, native_auth_document,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
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
    let outcome = fetch_and_persist_inner(state, source);
    let (result, status) = match &outcome {
        FetchAndPersistOutcome::Persisted(_) => ("persisted", None),
        FetchAndPersistOutcome::UpstreamError(error) => ("upstream_error", Some(error.status())),
        FetchAndPersistOutcome::Stopped => ("cancelled", None),
        FetchAndPersistOutcome::PersistenceError(PersistError::RuntimeChanged) => {
            ("runtime_changed", None)
        }
        FetchAndPersistOutcome::PersistenceError(PersistError::SourceChanged) => {
            ("source_changed", None)
        }
        FetchAndPersistOutcome::PersistenceError(PersistError::Internal) => {
            ("persistence_error", None)
        }
    };
    let journal = &state.backend.diagnostics.journal;
    journal.event(
        if matches!(result, "upstream_error" | "persistence_error") { "warning" } else { "info" },
        "catalog_refresh",
        &serde_json::json!({"source": journal.pseudonym(&source.id), "result": result, "http_status": status}),
    );
    outcome
}

fn fetch_and_persist_inner(state: &ServerState, source: &RefreshSource) -> FetchAndPersistOutcome {
    let request = fetch_catalog(&state.backend.transport.client, source);
    let fetched = state.backend.transport.runtime.block_on(async move {
        tokio::select! {
            biased;
            _ = state.wait_for_shutdown() => None,
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

pub(crate) fn persist_fetched_catalog(
    state: &ServerState,
    source: &RefreshSource,
    mut catalog: Value,
) -> Result<PersistedCatalog, PersistError> {
    let current_version = state
        .backend
        .integration
        .inventory
        .selected_trusted_version();
    if current_version.as_deref() != Some(source.client_version.as_str()) {
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
        source.path.clone()
    };

    let version_after_lock = state
        .backend
        .integration
        .inventory
        .selected_trusted_version();
    if version_after_lock.as_deref() != Some(source.client_version.as_str()) {
        return Err(PersistError::RuntimeChanged);
    }

    let previous = if source.native {
        emp_codex::load_native_catalog(&current)
    } else {
        super::read_account_cache(&path).unwrap_or_else(|| json!({}))
    };
    let retained = retain_known_models(&mut catalog, &previous, source);
    catalog["account_owner"] = json!(source.owner);
    catalog["base_url"] = json!(source.base);
    let changed = previous != catalog;
    if changed {
        state.catalog_refresh.mark_catalog_publication_pending();
        emp_state::filesystem::write_catalog_json(&path, &catalog)
            .map_err(|_| PersistError::Internal)?;
    }
    if source.native && emp_codex::preserve_native_catalog(&current).is_err() {
        return Err(PersistError::Internal);
    }
    if retained > 0 {
        let journal = &state.backend.diagnostics.journal;
        journal.event(
            "info",
            "catalog_models_retained",
            &json!({
                "source": journal.pseudonym(&source.id), "model_count": retained
            }),
        );
    }
    Ok(PersistedCatalog { catalog })
}

/// Catalog discovery is not an inference authorization check. A partial
/// response must not invalidate a previously selectable model mid-conversation.
/// Keep its metadata only for the same account/backend; fresh entries (including
/// explicit supported_in_api=false) always take precedence.
fn retain_known_models(catalog: &mut Value, previous: &Value, source: &RefreshSource) -> usize {
    let matches_source = |field: &str, expected: &str| {
        previous
            .get(field)
            .and_then(Value::as_str)
            .map_or(source.native && previous.get(field).is_none(), |value| {
                value == expected
            })
    };
    if !matches_source("account_owner", &source.owner) || !matches_source("base_url", &source.base)
    {
        return 0;
    }
    let (Some(known), Some(models)) = (
        previous.get("models").and_then(Value::as_array),
        catalog.get_mut("models").and_then(Value::as_array_mut),
    ) else {
        return 0;
    };
    let fresh_count = models.len();
    for model in known {
        if let Some(slug) = model.get("slug").and_then(Value::as_str)
            && !models.iter().any(|fresh| fresh["slug"] == slug)
        {
            models.push(model.clone());
        }
    }
    models.len() - fresh_count
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_do_not_cross_accounts_or_override_explicit_upstream_updates() {
        let source = RefreshSource {
            id: "@native".into(),
            path: PathBuf::from("native.json"),
            base: "https://catalog.example/v1".into(),
            headers: BTreeMap::new(),
            owner: "owner-a".into(),
            credential_fingerprint: String::new(),
            client_version: "0.159.2".into(),
            generation: 0,
            native: true,
            worker_eligible: false,
        };
        let mut previous = json!({"account_owner":"owner-a", "base_url":source.base,
            "models":[{"slug":"known"}, {"slug":"disabled","supported_in_api":true}]});
        for (owner, base, retain) in [
            ("owner-a", "https://catalog.example/v1", true),
            ("owner-b", "https://catalog.example/v1", false),
            ("owner-a", "https://other.example/v1", false),
        ] {
            previous["account_owner"] = json!(owner);
            previous["base_url"] = json!(base);
            let mut fresh = json!({"models":[{"slug":"disabled", "supported_in_api":false}]});
            retain_known_models(&mut fresh, &previous, &source);
            assert_eq!(
                fresh["models"].as_array().unwrap().len(),
                if retain { 2 } else { 1 }
            );
            assert_eq!(fresh["models"][0]["supported_in_api"], false);
        }
    }
}
