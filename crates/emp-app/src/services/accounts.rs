//! Account credentials, persistence and public snapshots.
use emp_codex::quota_history::QuotaHistoryStore;
use std::sync::{Arc, Condvar, Mutex};

use crate::app::ServerState;
use crate::http::auth::MAX_NATIVE_AUTH_BYTES;
use emp_codex::account_auth_headers;
use emp_codex::quota::QuotaError;
use emp_state::FileTransaction;
use emp_state::VaultStore;
use emp_state::duplicate_account_status;
use emp_state::normalize_account;
use emp_state::normalize_configuration;
use emp_state::public_configuration_with_file_status;
use emp_state::validate_auth_json;
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
    fs_metadata(path)
        .is_some_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn fs_metadata(path: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(path).ok()
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

pub(crate) fn import_account_state(state: &ServerState, body: &Value) -> Result<Value, String> {
    let metadata = body
        .as_object()
        .ok_or_else(|| "account import body must be an object".to_owned())?;
    let auth = metadata
        .get("auth_json")
        .ok_or_else(|| "auth_json must be a JSON object".to_owned())?;
    let auth = validate_auth_json(auth).map_err(|error| error.to_string())?;
    let account_id = metadata
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "account.id must be a safe single path segment".to_owned())?;
    // Quota refresh and rotated-credential flushes save credentials under
    // this lock; replacing them outside it could let an older rotated copy
    // overwrite the imported credential.
    let refresh_lock =
        quota_refresh_lock(state, account_id).ok_or_else(|| "internal server error".to_owned())?;
    let _refresh_guard = refresh_lock
        .lock()
        .map_err(|_| "internal server error".to_owned())?;
    let configured = |config: &Value| {
        config
            .get("accounts")
            .and_then(Value::as_array)
            .is_some_and(|accounts| {
                accounts
                    .iter()
                    .any(|item| item.get("id").and_then(Value::as_str) == Some(account_id))
            })
    };
    // Validates the import against `current` and builds the configuration to
    // store.
    let prepare = |current: &Value| -> Result<(PathBuf, Value), String> {
        let auth_path = emp_state::account_auth_path(
            current,
            account_id,
            &state.backend.configuration.config_path,
        )
        .map_err(|error| error.to_string())?;
        let raw = serde_json::json!({
            "id":metadata.get("id").cloned().unwrap_or(Value::Null),
            "name":metadata.get("name").cloned().unwrap_or_else(|| Value::String(account_id.to_owned())),
            "prefix":metadata.get("prefix").cloned().unwrap_or(Value::Null),
            "auth_file":auth_path.to_string_lossy(),
            "credential_status":"unknown",
            "enabled":metadata.get("enabled").cloned().unwrap_or(Value::Bool(true)),
            "hidden_models":metadata.get("hidden_models").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
            "model_context_windows":metadata.get("model_context_windows").cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new())),
        });
        let account = normalize_account(&raw).map_err(|error| error.to_string())?;
        let prefix = account
            .get("prefix")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let existing = current
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if existing.iter().any(|item| {
            item.get("id").and_then(Value::as_str) != Some(account_id)
                && item.get("prefix").and_then(Value::as_str) == Some(prefix)
        }) {
            return Err(format!("account prefix is already in use: {prefix}"));
        }
        let mut accounts = existing
            .into_iter()
            .filter(|item| item.get("id").and_then(Value::as_str) != Some(account_id))
            .collect::<Vec<_>>();
        accounts.push(account);
        let mut updated = current.clone();
        updated["accounts"] = Value::Array(accounts);
        let updated = normalize_configuration(Some(&updated)).map_err(|error| error.to_string())?;
        Ok((auth_path, updated))
    };
    // Held from reading the configuration to storing the result, like every
    // other configuration writer, so a concurrent writer's change is never
    // overwritten with this older copy. Validation, settling legacy history
    // and the commit all happen under it, so a request rejected by
    // validation never settles history.
    let mut config = state
        .backend
        .configuration
        .edit()
        .map_err(|_| "internal server error".to_owned())?;
    let (auth_path, mut updated) = prepare(&config)?;
    if configured(&config) {
        // The id is about to name new credentials: its legacy rows must be
        // attributed with the credentials that recorded them, or dropped.
        crate::services::quota::settle_legacy_quota_history(state, &config, account_id)
            .map_err(|_| "quota history is unavailable; the account was not replaced".to_owned())?;
    }
    let config_toml = auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.toml");
    let mut transaction = FileTransaction::new();
    transaction
        .remember(&auth_path)
        .map_err(|error| error.to_string())?;
    transaction
        .remember(&config_toml)
        .map_err(|error| error.to_string())?;
    state
        .backend
        .configuration
        .vault
        .write_encrypted_json(&auth_path, &auth)
        .map_err(|error| error.to_string())?;
    emp_state::atomic_write_private_state(&config_toml, b"cli_auth_credentials_store = \"file\"\n")
        .map_err(|error| error.to_string())?;
    let duplicates = duplicate_accounts(
        &updated,
        &state.backend.configuration.vault,
        &state.backend.accounts.native_auth_path,
    );
    (updated, _) = emp_state::migrate_duplicate_native_visibility(&updated, &duplicates);
    config
        .commit_files(&updated, transaction)
        .map_err(|error| error.to_string())?;
    forget_pending_rotation(state, &auth_path);
    state.catalog_refresh.account_changed(account_id);
    drop(config);
    notify_quota_update(state, account_id, None);
    account_public_snapshot(state, account_id).ok_or_else(|| "account import failed".to_owned())
}

pub(crate) fn delete_account_state(state: &ServerState, account_id: &str) -> Result<(), String> {
    let refresh_lock =
        quota_refresh_lock(state, account_id).ok_or_else(|| "internal server error".to_owned())?;
    let _refresh_guard = refresh_lock
        .lock()
        .map_err(|_| "internal server error".to_owned())?;
    // Held until the result is stored; see `import_account_state`.
    let mut config = state
        .backend
        .configuration
        .edit()
        .map_err(|_| "internal server error".to_owned())?;
    let current = config.clone();
    let accounts = current
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = accounts
        .iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
        .ok_or_else(|| format!("unknown account: {account_id}"))?;
    let expected = emp_state::account_auth_path(
        &current,
        account_id,
        &state.backend.configuration.config_path,
    )
    .map_err(|error| error.to_string())?;
    let configured = target
        .get("auth_file")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "refusing to delete credentials outside the account store".to_owned())?;
    if configured != expected {
        return Err("refusing to delete credentials outside the account store".to_owned());
    }
    // Legacy rows keyed by this id must reach the identity they belong to,
    // or be dropped, before the id becomes free for a different account.
    crate::services::quota::settle_legacy_quota_history(state, &config, account_id)
        .map_err(|_| "quota history is unavailable; the account was not deleted".to_owned())?;
    let mut updated = current.clone();
    updated["accounts"] = Value::Array(
        accounts
            .into_iter()
            .filter(|account| account.get("id").and_then(Value::as_str) != Some(account_id))
            .collect(),
    );
    if updated
        .get("subscription_search")
        .and_then(Value::as_object)
        .and_then(|search| search.get("account_id"))
        .and_then(Value::as_str)
        == Some(account_id)
        && let Some(search) = updated
            .get_mut("subscription_search")
            .and_then(Value::as_object_mut)
    {
        search.insert("enabled".to_owned(), Value::Bool(false));
        search.insert("account_id".to_owned(), Value::String(String::new()));
    }
    let config_toml = expected
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.toml");
    let mut transaction = FileTransaction::new();
    transaction
        .remember(&expected)
        .map_err(|error| error.to_string())?;
    transaction
        .remember(&config_toml)
        .map_err(|error| error.to_string())?;
    for path in [&expected, &config_toml] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    config
        .commit_files(&updated, transaction)
        .map_err(|error| error.to_string())?;
    forget_pending_rotation(state, &expected);
    state.catalog_refresh.account_changed(account_id);
    drop(config);
    notify_quota_update(state, account_id, None);
    Ok(())
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

/// Run `replace`, which may rewrite configured accounts' stored credentials
/// (migration import), under every configured account's refresh lock and
/// the configuration lock. `replace` edits the configuration in place, so
/// nothing another writer stored meanwhile is lost, and no account it sees
/// is unlocked. Rotated credentials whose stored file it rewrote are
/// dropped: the replacement is the user's explicit choice, and a flush must
/// not overwrite it later.
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

#[cfg(test)]
#[path = "accounts_delete_quota_race.rs"]
mod delete_quota_race;
