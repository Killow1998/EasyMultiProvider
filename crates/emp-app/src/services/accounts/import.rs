use super::{
    account_public_snapshot, forget_pending_rotation, native_auth_document, notify_quota_update,
    quota_refresh_lock,
};
use crate::app::ServerState;
use emp_state::{
    FileTransaction, VaultStore, normalize_account, normalize_configuration, validate_auth_json,
};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, atomic::AtomicBool};

struct ResolvedImport {
    metadata: serde_json::Map<String, Value>,
    auth: Value,
    account_id: String,
    identity_only: bool,
}

fn resolve(state: &ServerState, body: &Value) -> Result<ResolvedImport, String> {
    let mut metadata = body
        .as_object()
        .cloned()
        .ok_or("account import body must be an object")?;
    let auth = metadata
        .remove("auth_json")
        .ok_or("auth_json is required")?;
    let auth = validate_auth_json(&auth).map_err(|error| error.to_string())?;
    if let Some(id) = metadata.get("id") {
        let account_id = id
            .as_str()
            .ok_or("account.id must be a safe single path segment")?
            .to_owned();
        return Ok(ResolvedImport {
            metadata,
            auth,
            account_id,
            identity_only: false,
        });
    }
    let name = body["name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .ok_or("name is required")?;
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| "internal server error")?;
    let existing = matching_account(&config, &state.backend.configuration.vault, &auth);
    let mut resolved = existing.cloned().unwrap_or_else(|| serde_json::json!({}));
    if existing.is_none() {
        let reserved = config["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(config["providers"].as_array().into_iter().flatten());
        let used = reserved
            .flat_map(|record| [record["id"].as_str(), record["prefix"].as_str()])
            .flatten()
            .collect::<std::collections::BTreeSet<_>>();
        let id = loop {
            let candidate = format!(
                "account-{}",
                crate::util::random_hex(4).map_err(|_| "unable to allocate account reference")?
            );
            if !used.contains(candidate.as_str()) {
                break candidate;
            }
        };
        resolved["id"] = Value::String(id.clone());
        resolved["prefix"] = Value::String(id);
    }
    resolved["name"] = Value::String(name.trim().to_owned());
    if native_auth_document(&state.backend.accounts.native_auth_path)
        .is_some_and(|native| emp_state::same_account_auth(&native, &auth))
    {
        resolved["hidden_models"] = config["native_hidden_models"].clone();
        resolved["model_context_windows"] = config["native_model_context_windows"].clone();
    }
    Ok(ResolvedImport {
        account_id: resolved["id"].as_str().unwrap().to_owned(),
        metadata: resolved.as_object().unwrap().clone(),
        auth,
        identity_only: true,
    })
}

pub(crate) fn matching_account<'a>(
    config: &'a Value,
    vault: &VaultStore,
    auth: &Value,
) -> Option<&'a Value> {
    config["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|account| {
            account["auth_file"]
                .as_str()
                .and_then(|path| vault.read_encrypted_json(Path::new(path)).ok())
                .is_some_and(|old| emp_state::same_account_auth(&old, auth))
        })
}

pub(crate) fn import_account_state(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> Result<Value, String> {
    crate::services::observation::operation::observe(
        &state.backend.diagnostics,
        request_id,
        "account_import",
        |receipt| {
            let ResolvedImport {
                metadata,
                auth,
                account_id,
                identity_only,
            } = resolve(state, body)?;
            let account_id = account_id.as_str();
            // Serialize credential writes with quota refresh and token rotation.
            receipt.subject(account_id);
            let refresh_lock = quota_refresh_lock(state, account_id)
                .ok_or_else(|| "internal server error".to_owned())?;
            let _refresh_guard = refresh_lock
                .lock()
                .map_err(|_| "internal server error".to_owned())?;
            let mut config = state
                .backend
                .configuration
                .edit()
                .map_err(|_| "internal server error".to_owned())?;
            if identity_only
                && let Some(existing) = config["accounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|record| record["id"] == account_id)
            {
                let same = existing["auth_file"]
                    .as_str()
                    .and_then(|path| {
                        state
                            .backend
                            .configuration
                            .vault
                            .read_encrypted_json(Path::new(path))
                            .ok()
                    })
                    .is_some_and(|old| emp_state::same_account_auth(&old, &auth));
                if !same {
                    return Err(
                        "account identity changed during import; please import again".into(),
                    );
                }
            }
            let auth_path = emp_state::account_auth_path(
                &config,
                account_id,
                &state.backend.configuration.config_path,
            )
            .map_err(|error| error.to_string())?;
            let raw = serde_json::json!({
                "id":account_id,
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
            let existing = config
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
            let mut updated = config.clone();
            updated["accounts"] = Value::Array(accounts);
            let updated =
                normalize_configuration(Some(&updated)).map_err(|error| error.to_string())?;
            if config["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|account| account["id"] == account_id)
            {
                // Attribute legacy usage before replacing its credentials.
                receipt
                    .step("settle_history", || {
                        crate::services::quota::settle_legacy_quota_history(
                            state, &config, account_id,
                        )
                    })
                    .map_err(|_| {
                        "quota history is unavailable; the account was not replaced".to_owned()
                    })?;
            }
            receipt.step("commit_account_and_configuration", || {
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
                emp_state::atomic_write_private_state(
                    &config_toml,
                    b"cli_auth_credentials_store = \"file\"\n",
                )
                .map_err(|error| error.to_string())?;
                config
                    .commit_files(&updated, transaction)
                    .map_err(|error| error.to_string())
            })?;
            receipt.check("account_and_configuration_committed", Some(true));
            forget_pending_rotation(state, &auth_path);
            if let Ok(mut cancellations) = state.backend.accounts.quota_refresh_cancellations.lock()
            {
                cancellations.insert(account_id.to_owned(), Arc::new(AtomicBool::new(false)));
            }
            state.catalog_refresh.account_changed(account_id);
            drop(config);
            notify_quota_update(state, account_id, None);
            receipt.step("read_account_result", || {
                account_public_snapshot(state, account_id)
                    .ok_or_else(|| "account import failed".to_owned())
            })
        },
    )
}
