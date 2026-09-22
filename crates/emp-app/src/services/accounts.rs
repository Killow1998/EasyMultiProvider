//! Account credentials, persistence and public snapshots.
use emp_codex::quota_history::QuotaHistoryStore;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Condvar, Mutex};

use crate::app::ServerState;
use crate::http::auth::MAX_NATIVE_AUTH_BYTES;
use emp_codex::account_auth_headers;
use emp_codex::quota::QuotaError;
use emp_state::FileTransaction;
use emp_state::VaultStore;
use emp_state::load_configuration;
use emp_state::normalize_account;
use emp_state::normalize_configuration;
use emp_state::public_configuration_with_file_status;
use emp_state::save_configuration_in_transaction;
use emp_state::validate_auth_json;
use emp_state::{duplicate_account_status, same_account_auth};
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
    let config = state.backend.configuration.config.lock().ok()?.clone();
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
    let current = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())?
        .clone();
    let account_id = metadata
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "account.id must be a safe single path segment".to_owned())?;
    let auth_path = emp_state::account_auth_path(
        &current,
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
    accounts.push(account.clone());
    let mut updated = current.clone();
    updated["accounts"] = Value::Array(accounts);
    let mut updated = normalize_configuration(Some(&updated)).map_err(|error| error.to_string())?;
    let config_toml = auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.toml");
    let mut transaction = FileTransaction::new();
    let operation = (|| -> Result<Value, String> {
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
        std::fs::write(&config_toml, b"cli_auth_credentials_store = \"file\"\n")
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&config_toml, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        let duplicates = duplicate_accounts(
            &updated,
            &state.backend.configuration.vault,
            &state.backend.accounts.native_auth_path,
        );
        (updated, _) = emp_state::migrate_duplicate_native_visibility(&updated, &duplicates);
        save_configuration_in_transaction(
            &updated,
            Some(&state.backend.configuration.config_path),
            &state.backend.configuration.vault,
            &mut transaction,
        )
        .map_err(|error| error.to_string())?;
        load_configuration(Some(&state.backend.configuration.config_path))
            .map_err(|error| error.to_string())
    })();
    let committed = match operation {
        Ok(config) => {
            transaction.commit();
            config
        }
        Err(error) => {
            let _ = transaction.rollback();
            return Err(error);
        }
    };
    *state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())? = committed;
    notify_quota_update(state, account_id, None);
    account_public_snapshot(state, account_id).ok_or_else(|| "account import failed".to_owned())
}

pub(crate) fn delete_account_state(state: &ServerState, account_id: &str) -> Result<(), String> {
    let current = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())?
        .clone();
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
    let owner = quota_owner_key(state, account_id).map_err(|error| error.to_string())?;
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
    let operation = (|| -> Result<Value, String> {
        transaction
            .remember(&expected)
            .map_err(|error| error.to_string())?;
        transaction
            .remember(&config_toml)
            .map_err(|error| error.to_string())?;
        save_configuration_in_transaction(
            &updated,
            Some(&state.backend.configuration.config_path),
            &state.backend.configuration.vault,
            &mut transaction,
        )
        .map_err(|error| error.to_string())?;
        for path in [&expected, &config_toml] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        load_configuration(Some(&state.backend.configuration.config_path))
            .map_err(|error| error.to_string())
    })();
    let committed = match operation {
        Ok(config) => {
            transaction.commit();
            config
        }
        Err(error) => {
            let _ = transaction.rollback();
            return Err(error);
        }
    };
    *state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())? = committed;
    if owner == account_id {
        state
            .backend
            .accounts
            .quota_history
            .delete_account(account_id)
            .map_err(|error| error.to_string())?;
    }
    notify_quota_update(state, account_id, None);
    Ok(())
}

pub(crate) struct AccountState {
    pub(crate) native_auth_path: PathBuf,
    pub(crate) codex_home: PathBuf,
    pub(crate) codex_binary: String,
    pub(crate) native_quota: Mutex<Option<Value>>,
    pub(crate) quota_refresh_errors: Mutex<BTreeMap<String, String>>,
    pub(crate) quota_refresh_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    pub(crate) quota_history: QuotaHistoryStore,
    pub(crate) quota_revision: Mutex<u64>,
    pub(crate) quota_condition: Condvar,
    pub(crate) quota_event_slots: AtomicUsize,
    pub(crate) quota_sampler_wait: Mutex<()>,
    pub(crate) quota_sampler_condition: Condvar,
}

pub(crate) fn duplicate_accounts(
    config: &Value,
    vault: &VaultStore,
    native_auth_path: &Path,
) -> BTreeMap<String, String> {
    let native = native_auth_document(native_auth_path);
    let credentials = config
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
        .collect::<Vec<_>>();
    duplicate_account_status(native.as_ref(), &credentials)
}

pub(crate) fn quota_owner_key(state: &ServerState, account_id: &str) -> Result<String, QuotaError> {
    if account_id == "@native" {
        return Ok(account_id.to_owned());
    }
    let accounts = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !accounts
        .iter()
        .any(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
    {
        return Err(QuotaError::new(
            format!("unknown account: {account_id}"),
            "quota_error",
        ));
    }
    let mut owners = Vec::<(String, Value)>::new();
    if let Some(native) = native_auth_document(&state.backend.accounts.native_auth_path) {
        owners.push(("@native".to_owned(), native));
    }
    for account in accounts {
        let Some(id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        let auth = account
            .get("auth_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .and_then(|path| {
                state
                    .backend
                    .configuration
                    .vault
                    .read_encrypted_json(Path::new(path))
                    .ok()
            });
        let source = auth.as_ref().and_then(|auth| {
            owners
                .iter()
                .find(|(_, seen)| same_account_auth(auth, seen))
                .map(|(owner, _)| owner.clone())
        });
        if id == account_id {
            return Ok(source.unwrap_or_else(|| id.to_owned()));
        }
        if source.is_none()
            && let Some(auth) = auth
        {
            owners.push((id.to_owned(), auth));
        }
    }
    Ok(account_id.to_owned())
}

pub(crate) fn notify_quota_update(state: &ServerState, account_id: &str, error: Option<&str>) {
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
    if let Ok(mut revision) = state.backend.accounts.quota_revision.lock() {
        *revision = revision.wrapping_add(1);
        state.backend.accounts.quota_condition.notify_all();
    }
}
