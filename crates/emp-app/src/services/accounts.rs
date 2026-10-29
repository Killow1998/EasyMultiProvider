//! Account credentials, persistence and public snapshots.
use emp_codex::quota_history::QuotaHistoryStore;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};

use crate::app::ServerState;
use crate::http::auth::MAX_NATIVE_AUTH_BYTES;
use emp_codex::account_auth_headers;
use emp_codex::quota::QuotaError;
mod delete;
mod import;
pub(crate) use delete::delete_account_state;
use emp_state::VaultStore;
use emp_state::duplicate_account_status;
use emp_state::public_configuration_with_file_status;
use emp_state::validate_auth_json;
pub(crate) use import::{import_account_state, matching_account};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn account_catalog_headers(
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
        .accounts
        .native_quota
        .lock()
        .ok()
        .and_then(|quota| quota.clone())
        .unwrap_or(Value::Null);
    serde_json::json!({
        "id": "@native",
        "name": "Current Codex login",
        "prefix": "",
        "native": true,
        "auth_mode": emp_integration::local_auth::mode(&state.backend.accounts.native_auth_path),
        "credential_set": regular_file(&state.backend.accounts.native_auth_path),
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

pub(crate) fn accounts_snapshot(state: &ServerState) -> Option<Value> {
    let config = state.backend.configuration.read().ok()?.clone();
    let duplicates = duplicate_accounts(
        &config,
        &state.backend.configuration.vault,
        &state.backend.accounts.native_auth_path,
    );
    let public = public_configuration_with_file_status(&config, &duplicates, regular_file).ok()?;
    let errors = state
        .backend
        .accounts
        .quota_refresh_errors
        .lock()
        .ok()
        .map(|errors| {
            errors
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect::<serde_json::Map<_, _>>()
        })?;
    Some(serde_json::json!({
        "native_account": native_account_snapshot(state, &config),
        "accounts": public
            .get("accounts")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "refresh_errors": errors,
        "claude_quota": state.backend.claude_quota.snapshot(crate::util::system_now()),
        "memory_routing_status": state.backend.memory_status.snapshot(),
    }))
}

pub(crate) fn account_public_snapshot(state: &ServerState, account_id: &str) -> Option<Value> {
    accounts_snapshot(state)?
        .get("accounts")?
        .as_array()?
        .iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
        .cloned()
}

pub(crate) fn native_auth_document(path: &Path) -> Option<Value> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_NATIVE_AUTH_BYTES as u64
    {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
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
        // Refresh locks before the configuration lock, the order quota
        // refreshes use.
        let locks = account_ids
            .iter()
            .map(|id| quota_refresh_lock(state, id))
            .collect::<Option<Vec<_>>>()?;
        let guards = locks
            .iter()
            .map(|lock| lock.lock().ok())
            .collect::<Option<Vec<_>>>()?;
        let mut config = configuration.edit().ok()?;
        // An account added between listing and locking would run
        // unprotected: lock again until every configured account is held.
        let current_ids = configured_ids(&config);
        if !current_ids.iter().all(|id| account_ids.contains(id)) {
            drop(config);
            drop(guards);
            account_ids = current_ids;
            continue;
        }
        let pending_files = state
            .backend
            .accounts
            .pending_rotations
            .lock()
            .ok()?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
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
            state.catalog_refresh.account_changed(id);
        }
        for (path, before) in pending_files.iter().zip(before) {
            if std::fs::read(path).ok() != before {
                forget_pending_rotation(state, Path::new(path));
            }
        }
        drop(config);
        drop(guards);
        return Some(result);
    }
}

/// Drop a rotated credential awaiting a durable save: the account's stored
/// credentials were replaced or removed, so the pending copy is stale.
pub(crate) fn forget_pending_rotation(state: &ServerState, auth_path: &Path) {
    if let Ok(mut pending) = state.backend.accounts.pending_rotations.lock() {
        pending.remove(auth_path.to_string_lossy().as_ref());
    }
}

pub(crate) struct AccountState {
    pub(crate) native_auth_path: PathBuf,
    pub(crate) codex_home: PathBuf,
    pub(crate) codex_binary: String,
    pub(crate) native_quota: Mutex<Option<Value>>,
    pub(crate) quota_refresh_errors: Mutex<BTreeMap<String, String>>,
    pub(crate) quota_refresh_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    pub(crate) quota_refresh_cancellations: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    pub(crate) quota_history: QuotaHistoryStore,
    pub(crate) quota_sampler_wait: Mutex<()>,
    pub(crate) quota_sampler_condition: Condvar,
    /// Rotated credentials whose durable save failed, keyed by encrypted
    /// auth file. Codex may already have invalidated the stored refresh token,
    /// so the next quota check uses (and re-saves) this copy instead.
    pub(crate) pending_rotations: Mutex<BTreeMap<String, Value>>,
    /// Quota checks and resets that may rotate a credential upstream.
    /// Shutdown closes it and waits for them before the final save.
    pub(crate) credential_operations: CredentialOperationGate,
}

/// Counts in-flight credential operations and refuses new ones once closed.
#[derive(Default)]
pub(crate) struct CredentialOperationGate {
    state: Mutex<(usize, bool)>,
    idle: Condvar,
}

/// Held while an operation may rotate a credential; see
/// [`CredentialOperationGate`].
pub(crate) struct CredentialOperation<'a>(&'a CredentialOperationGate);

impl CredentialOperationGate {
    /// `None` once the gate is closed for shutdown.
    pub(crate) fn enter(&self) -> Option<CredentialOperation<'_>> {
        let mut state = self.state.lock().ok()?;
        if state.1 {
            return None;
        }
        state.0 += 1;
        Some(CredentialOperation(self))
    }

    /// Refuse new operations and wait up to `timeout` for running ones.
    /// Returns how many are still running: a credential one of them rotated
    /// may never be saved.
    pub(crate) fn close_and_drain(&self, timeout: std::time::Duration) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        state.1 = true;
        let deadline = std::time::Instant::now() + timeout;
        while state.0 > 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            state = match self.idle.wait_timeout(state, remaining) {
                Ok((state, _)) => state,
                Err(_) => return 0,
            };
        }
        state.0
    }
}

impl Drop for CredentialOperation<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.0 -= 1;
            if state.0 == 0 {
                self.0.idle.notify_all();
            }
        }
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
    config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
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

/// [`quota_owner_key`] against `config`, for callers already holding the
/// configuration lock.
pub(crate) fn quota_owner_key_in(
    state: &ServerState,
    config: &Value,
    account_id: &str,
) -> Result<String, QuotaError> {
    let auth = if account_id == "@native" {
        native_auth_document(&state.backend.accounts.native_auth_path)
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
#[path = "accounts_delete_quota_race.rs"]
mod delete_quota_race;
