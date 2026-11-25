use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::app::ServerState;
use crate::services::configuration::{CommittedChange, after_commit};
use emp_codex::account_auth_headers;
use emp_codex::quota::QuotaError;
mod credentials;
mod delete;
mod import;
pub(super) use credentials::AccountCredentials;
use credentials::CredentialOperationGate;
pub(crate) use credentials::flush_pending_rotations;
pub(crate) use delete::delete_account_state;
use emp_state::VaultStore;
use emp_state::duplicate_account_status;
use emp_state::validate_auth_json;
pub(crate) use import::{
    AccountImport, import_account_state, import_emp_login_account_state, import_parsed_account,
    matching_account,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) fn account_allows_refresh(state: &ServerState, account_id: &str) -> bool {
    !state.backend.team.shared_subscription(account_id)
        && state.backend.configuration.read().is_ok_and(|config| {
            config["accounts"].as_array().is_some_and(|accounts| {
                accounts.iter().any(|account| {
                    account["id"] == account_id
                        && account["credential_source"] == "emp_login"
                        && account["workspace_shared"] != true
                })
            })
        })
}

pub(crate) fn subscription_auth_headers(
    account: &serde_json::Map<String, Value>,
    vault: &VaultStore,
) -> Option<BTreeMap<String, String>> {
    let path = account
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())?;
    let auth = vault.read_encrypted_json(Path::new(path)).ok()?;
    account_auth_headers(&auth)
}

pub(crate) fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

pub(crate) fn native_account_snapshot(state: &ServerState, config: &Value) -> Value {
    let quota = state
        .backend
        .quota
        .with_native_quota(Value::clone)
        .unwrap_or(Value::Null);
    serde_json::json!({
        "id": "@native",
        "name": "Current Codex login",
        "prefix": "",
        "native": true,
        "auth_mode": emp_codex::configuration::local_auth::mode(&state.backend.codex.auth_path),
        "credential_set": regular_file(&state.backend.codex.auth_path),
        "hidden_models": config
            .get("native_hidden_models")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "model_context_windows": config
            .get("native_model_context_windows")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
        "quota": quota,
    })
}

pub(crate) fn project_public_accounts(state: &ServerState, config: &mut Value) -> Vec<Value> {
    state
        .backend
        .team
        .project_subscription_credentials(state, config);
    let duplicates = duplicate_accounts(
        config,
        &state.backend.configuration.vault,
        &state.backend.codex.auth_path,
    );
    let mut accounts = emp_state::config::public_accounts(config, &duplicates);
    state
        .backend
        .team
        .label_subscription_permissions(&mut accounts);
    let Value::Array(accounts) = accounts else {
        unreachable!("public account list");
    };
    accounts
}

pub(crate) fn account_public_snapshot(state: &ServerState, account_id: &str) -> Option<Value> {
    let mut config = state.backend.configuration.snapshot().ok()?;
    project_public_accounts(state, &mut config)
        .into_iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
}

pub(crate) fn native_auth_document(path: &Path) -> Option<Value> {
    emp_codex::configuration::local_auth::read(path)
        .ok()
        .flatten()
}

pub(crate) fn native_subscription_headers(path: &Path) -> Option<BTreeMap<String, String>> {
    native_auth_document(path)
        .and_then(|auth| validate_auth_json(&auth).ok())
        .and_then(|auth| account_auth_headers(&auth))
}

/// Import an already decrypted bundle under the account and configuration
/// ownership rules. HTTP callers do not acquire locks or publish snapshots.
pub(crate) fn import_account_bundle(
    state: &ServerState,
    decrypted: emp_state::migration::DecryptedMigration,
) -> Option<
    Result<emp_state::migration::MigrationImportSummary, emp_state::migration::MigrationError>,
> {
    replacing_account_credentials(state, |config| config.import_migration(decrypted))
}

/// Hold account refresh locks before the configuration lock, then discard
/// pending rotations for credentials replaced by migration.
pub(super) fn replacing_account_credentials<T>(
    state: &ServerState,
    replace: impl FnOnce(&mut super::configuration::ConfigurationEdit<'_>) -> T,
) -> Option<T> {
    let configured_ids = |config: &Value| {
        let mut ids = config
            .get("accounts")
            .and_then(Value::as_array)
            .map(|accounts| {
                accounts
                    .iter()
                    .filter_map(|account| account.get("id")?.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        ids
    };
    let configuration = &state.backend.configuration;
    let mut account_ids = configured_ids(&configuration.snapshot().ok()?);
    loop {
        let locks = quota_refresh_locks(state, account_ids.iter().map(String::as_str))?;
        let guards = locks
            .iter()
            .map(|lock| lock.lock().ok())
            .collect::<Option<Vec<_>>>()?;
        let mut config = configuration.edit().ok()?;
        let current_ids = configured_ids(&config);
        if !current_ids.iter().all(|id| account_ids.contains(id)) {
            drop(config);
            drop(guards);
            account_ids = current_ids;
            continue;
        }
        let pending_files = state.backend.accounts.pending_rotation_paths().ok()?;
        let before = pending_files
            .iter()
            .map(|path| std::fs::read(path).ok())
            .collect::<Vec<_>>();
        let previous_ids = configured_ids(&config);
        let result = replace(&mut config);
        let current_ids = configured_ids(&config);
        if let Ok(mut cancellations) = state.backend.accounts.quota_refresh_cancellations.lock() {
            for id in current_ids.iter().filter(|id| !previous_ids.contains(id)) {
                cancellations.insert(id.clone(), Arc::new(AtomicBool::new(false)));
            }
        }
        for id in previous_ids.iter().chain(current_ids.iter()) {
            let _ = after_commit(state, CommittedChange::AccountSource(id));
        }
        for (path, before) in pending_files.iter().zip(before) {
            if std::fs::read(path).ok() != before {
                state
                    .backend
                    .accounts
                    .forget_pending_rotation(Path::new(path));
            }
        }
        drop(config);
        drop(guards);
        return Some(result);
    }
}

#[derive(Default)]
pub(crate) struct AccountState {
    quota_refresh_errors: Mutex<BTreeMap<String, String>>,
    quota_refresh_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    quota_refresh_cancellations: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    /// Kept by encrypted auth file because rotation may invalidate the stored refresh token.
    pending_rotations: Mutex<BTreeMap<String, Value>>,
    credential_operations: CredentialOperationGate,
}

impl AccountState {
    pub(crate) fn refresh_errors(&self, config: &Value) -> Option<Map<String, Value>> {
        let mut errors = self.quota_refresh_errors.lock().ok().map(|errors| {
            errors
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect::<Map<_, _>>()
        })?;
        for account in config["accounts"].as_array().expect("normalized accounts") {
            if let Some(source) = account["_emp_shared_source"].as_str() {
                let id = account["id"].as_str().expect("normalized account ID");
                errors.remove(id);
                if let Some(error) = errors.get(source).cloned() {
                    errors.insert(id.to_owned(), error);
                }
            }
        }
        Some(errors)
    }
}

pub(crate) fn duplicate_accounts(
    config: &Value,
    vault: &VaultStore,
    native_auth_path: &Path,
) -> BTreeMap<String, String> {
    let native = native_auth_document(native_auth_path);
    duplicate_account_status(native.as_ref(), &account_credentials(config, vault))
}

pub(super) fn account_credentials(config: &Value, vault: &VaultStore) -> Vec<(String, Value)> {
    let accounts = config["accounts"].as_array().expect("normalized accounts");
    let (shared, private): (Vec<_>, Vec<_>) = accounts
        .iter()
        .partition(|account| account["workspace_shared"] == true);
    private
        .into_iter()
        .chain(shared)
        .filter_map(|account| {
            let id = account.get("id")?.as_str()?;
            let path = account.get("auth_file")?.as_str()?;
            if path.is_empty() {
                return None;
            }
            vault
                .read_encrypted_json(Path::new(path))
                .ok()
                .map(|auth| (id.to_owned(), auth))
        })
        .collect()
}

pub(crate) fn quota_owner_key(state: &ServerState, account_id: &str) -> Result<String, QuotaError> {
    if account_id == "@native" {
        return quota_owner_key_in(state, &Value::Null, account_id);
    }
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .clone();
    quota_owner_key_in(state, &config, account_id)
}

/// [`quota_owner_key`] for callers already holding the configuration lock.
pub(crate) fn quota_owner_key_in(
    state: &ServerState,
    config: &Value,
    account_id: &str,
) -> Result<String, QuotaError> {
    let auth = if account_id == "@native" {
        native_auth_document(&state.backend.codex.auth_path)
    } else {
        let account = config
            .get("accounts")
            .and_then(Value::as_array)
            .and_then(|accounts| {
                accounts
                    .iter()
                    .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
            })
            .ok_or_else(|| {
                QuotaError::new(format!("unknown account: {account_id}"), "quota_error")
            })?;
        let path = account
            .get("auth_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                QuotaError::new(
                    "Subscription account authentication is unavailable",
                    "quota_error",
                )
            })?;
        state
            .backend
            .configuration
            .vault
            .read_encrypted_json(Path::new(path))
            .ok()
    }
    .ok_or_else(|| QuotaError::new("Account authentication is unavailable", "quota_error"))?;
    let headers = account_auth_headers(&auth)
        .ok_or_else(|| QuotaError::new("Account authentication is unavailable", "quota_error"))?;
    let owner = emp_state::usage::account_owner(&headers);
    if owner.is_empty() {
        return Err(QuotaError::new(
            "Account identity is unavailable",
            "quota_error",
        ));
    }
    Ok(owner)
}

pub(crate) fn notify_quota_update(state: &ServerState, account_id: &str, error: Option<&str>) {
    if error.is_none_or(str::is_empty) {
        state.backend.auto_review.clear(account_id);
    }
    if let Ok(mut errors) = state.backend.accounts.quota_refresh_errors.lock() {
        match error {
            Some(error) => {
                errors.insert(account_id.to_owned(), error.to_owned());
            }
            None => {
                errors.remove(account_id);
            }
        }
    }
    state
        .backend
        .management_events
        .publish(super::management_events::Change::Quota);
}

pub(crate) fn quota_refresh_lock(state: &ServerState, account_id: &str) -> Option<Arc<Mutex<()>>> {
    let mut locks = state.backend.accounts.quota_refresh_locks.lock().ok()?;
    Some(Arc::clone(
        locks
            .entry(account_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    ))
}

pub(super) fn quota_refresh_locks<'a>(
    state: &ServerState,
    account_ids: impl IntoIterator<Item = &'a str>,
) -> Option<Vec<Arc<Mutex<()>>>> {
    let mut account_ids = account_ids.into_iter().collect::<Vec<_>>();
    account_ids.sort_unstable();
    account_ids.dedup();
    account_ids
        .into_iter()
        .map(|id| quota_refresh_lock(state, id))
        .collect()
}

pub(crate) fn quota_refresh_cancellation(
    state: &ServerState,
    account_id: &str,
) -> Option<Arc<AtomicBool>> {
    let mut flags = state
        .backend
        .accounts
        .quota_refresh_cancellations
        .lock()
        .ok()?;
    Some(Arc::clone(
        flags
            .entry(account_id.to_owned())
            .or_insert_with(|| Arc::new(AtomicBool::new(false))),
    ))
}

#[cfg(test)]
mod tests;
