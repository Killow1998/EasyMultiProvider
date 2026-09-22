//! Account-scoped quota refresh, reset and history operations.

use crate::app::ServerState;
use crate::services::accounts::account_public_snapshot;
use crate::services::accounts::native_account_snapshot;
use crate::services::accounts::native_auth_document;
use crate::services::accounts::regular_file;
use crate::services::accounts::{notify_quota_update, quota_owner_key};
use crate::util::system_now;
use emp_codex::quota::QuotaError;
use emp_codex::quota::consume_native_quota_reset;
use emp_codex::quota::read_native_login_quota;
use emp_codex::quota::run_quota_query_persisting;
use emp_codex::quota::run_quota_reset_persisting;
use emp_codex::quota_history::QuotaHistoryError;
use emp_state::load_configuration;
use emp_state::same_account_auth;
use emp_state::save_configuration;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

fn save_account_quota_state(
    state: &ServerState,
    account_id: &str,
    auth_file: &str,
    status: &str,
    quota: Option<&Value>,
) -> Result<Value, QuotaError> {
    let mut config = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    let Some(account) = config
        .get_mut("accounts")
        .and_then(Value::as_array_mut)
        .and_then(|accounts| {
            accounts.iter_mut().find(|account| {
                account.get("id").and_then(Value::as_str) == Some(account_id)
                    && account.get("auth_file").and_then(Value::as_str) == Some(auth_file)
            })
        })
    else {
        return Err(QuotaError::new(
            "account changed during quota refresh",
            "quota_error",
        ));
    };
    account["credential_status"] = Value::String(status.to_owned());
    if let Some(quota) = quota {
        account["quota"] = quota.clone();
    }
    save_configuration(
        &config,
        Some(&state.backend.configuration.config_path),
        &state.backend.configuration.vault,
    )
    .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    *config = load_configuration(Some(&state.backend.configuration.config_path))
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    drop(config);
    account_public_snapshot(state, account_id)
        .ok_or_else(|| QuotaError::new("account changed during quota refresh", "quota_error"))
}

fn record_quota_snapshot(state: &ServerState, account_id: &str, quota: &Value) {
    let Ok(owner) = quota_owner_key(state, account_id) else {
        return;
    };
    let _ = state.backend.accounts.quota_history.append_snapshot(
        &owner,
        quota,
        system_now().trunc() as i64,
    );
}

fn refresh_imported_account(state: &ServerState, account_id: &str) -> Result<Value, QuotaError> {
    let target = state
        .backend
        .configuration
        .config
        .lock()
        .ok()
        .and_then(|config| {
            config
                .get("accounts")?
                .as_array()?
                .iter()
                .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
                .cloned()
        })
        .ok_or_else(|| QuotaError::new(format!("unknown account: {account_id}"), "quota_error"))?;
    let auth_file = target
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| QuotaError::new("account credentials are not configured", "quota_error"))?
        .to_owned();
    let auth_path = Path::new(&auth_file);
    let read_auth = || {
        state
            .backend
            .configuration
            .vault
            .read_encrypted_json(auth_path)
            .map_err(|_| QuotaError::new("stored encrypted auth.json is invalid", "quota_error"))
    };
    let query = |auth: &Value, allow_refresh: bool| {
        run_quota_query_persisting(
            auth,
            &state.backend.accounts.codex_binary,
            Duration::from_secs(45),
            allow_refresh,
            |refreshed| {
                state
                    .backend
                    .configuration
                    .vault
                    .write_encrypted_json(auth_path, refreshed)
                    .map_err(|_| ())
            },
        )
    };
    let auth = read_auth()?;
    if native_auth_document(&state.backend.accounts.native_auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return match read_native_login_quota(
            &state.backend.accounts.native_auth_path,
            &state.backend.accounts.codex_binary,
            Duration::from_secs(45),
        ) {
            Ok(quota) => {
                save_account_quota_state(state, account_id, &auth_file, "valid", Some(&quota))
                    .inspect(|_| record_quota_snapshot(state, account_id, &quota))
            }
            Err(error) => {
                if error.code() == "quota_auth_required" {
                    let _ =
                        save_account_quota_state(state, account_id, &auth_file, "invalid", None);
                }
                Err(error)
            }
        };
    }
    let quota = match query(&auth, false) {
        Ok(quota) => quota,
        Err(error) if error.code() == "quota_auth_required" => {
            let refreshed = read_auth()?;
            match query(&refreshed, true) {
                Ok(quota) => quota,
                Err(error) => {
                    if error.code() == "quota_auth_required" {
                        let _ = save_account_quota_state(
                            state, account_id, &auth_file, "invalid", None,
                        );
                    }
                    return Err(error);
                }
            }
        }
        Err(error) => return Err(error),
    };
    save_account_quota_state(state, account_id, &auth_file, "valid", Some(&quota))
        .inspect(|_| record_quota_snapshot(state, account_id, &quota))
}

fn refresh_account_by_id_inner(state: &ServerState, account_id: &str) -> Result<Value, QuotaError> {
    if account_id != "@native" {
        return refresh_imported_account(state, account_id);
    }
    let quota = read_native_login_quota(
        &state.backend.accounts.native_auth_path,
        &state.backend.accounts.codex_binary,
        Duration::from_secs(45),
    )?;
    if let Ok(mut current) = state.backend.accounts.native_quota.lock() {
        *current = Some(quota.clone());
    } else {
        return Err(QuotaError::new(
            "Codex account quota check failed",
            "quota_error",
        ));
    }
    let config = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .clone();
    record_quota_snapshot(state, account_id, &quota);
    Ok(native_account_snapshot(state, &config))
}

pub(crate) fn refresh_account_by_id(
    state: &ServerState,
    account_id: &str,
) -> Result<Value, QuotaError> {
    let result = refresh_account_by_id_inner(state, account_id);
    notify_quota_update(
        state,
        account_id,
        result.as_ref().err().map(|error| error.code()),
    );
    result
}

pub(crate) fn quota_history_response(
    state: &ServerState,
    account_id: &str,
    range_name: &str,
    now: i64,
) -> Result<Value, QuotaHistoryResponseError> {
    let owner = quota_owner_key(state, account_id).map_err(QuotaHistoryResponseError::Account)?;
    let mut result = state
        .backend
        .accounts
        .quota_history
        .query(&owner, range_name, now)
        .map_err(QuotaHistoryResponseError::History)?;
    result["account_id"] = Value::String(account_id.to_owned());
    Ok(result)
}

pub(crate) enum QuotaHistoryResponseError {
    Account(QuotaError),
    History(QuotaHistoryError),
}

pub(crate) fn consume_quota_reset_for_account(
    state: &ServerState,
    account_id: &str,
    idempotency_key: &str,
) -> Result<String, QuotaError> {
    if account_id == "@native" {
        return consume_native_quota_reset(
            &state.backend.accounts.native_auth_path,
            &state.backend.accounts.codex_binary,
            Duration::from_secs(45),
            idempotency_key,
        );
    }
    let target = state
        .backend
        .configuration
        .config
        .lock()
        .ok()
        .and_then(|config| {
            config
                .get("accounts")?
                .as_array()?
                .iter()
                .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
                .cloned()
        })
        .ok_or_else(|| QuotaError::new(format!("unknown account: {account_id}"), "quota_error"))?;
    let auth_file = target
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| QuotaError::new("account credentials are not configured", "quota_error"))?
        .to_owned();
    let auth_path = Path::new(&auth_file);
    let read_auth = || {
        state
            .backend
            .configuration
            .vault
            .read_encrypted_json(auth_path)
            .map_err(|_| QuotaError::new("stored encrypted auth.json is invalid", "quota_error"))
    };
    let auth = read_auth()?;
    if native_auth_document(&state.backend.accounts.native_auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return consume_native_quota_reset(
            &state.backend.accounts.native_auth_path,
            &state.backend.accounts.codex_binary,
            Duration::from_secs(45),
            idempotency_key,
        );
    }
    let query = |auth: &Value, allow_refresh: bool| {
        run_quota_reset_persisting(
            auth,
            &state.backend.accounts.codex_binary,
            Duration::from_secs(45),
            allow_refresh,
            idempotency_key,
            |refreshed| {
                state
                    .backend
                    .configuration
                    .vault
                    .write_encrypted_json(auth_path, refreshed)
                    .map_err(|_| ())
            },
        )
    };
    match query(&auth, false) {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.code() == "quota_auth_required" => query(&read_auth()?, true),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QuotaSampleCounts {
    pub(crate) sampled: usize,
    pub(crate) failed: usize,
}

fn quota_sample_targets(state: &ServerState) -> Vec<String> {
    let mut targets = Vec::new();
    if regular_file(&state.backend.accounts.native_auth_path) {
        targets.push("@native".to_owned());
    }
    let accounts = state
        .backend
        .configuration
        .config
        .lock()
        .ok()
        .and_then(|config| config.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    for account in accounts {
        let Some(account_id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        if account
            .get("auth_file")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            continue;
        }
        if quota_owner_key(state, account_id).as_deref() == Ok(account_id) {
            targets.push(account_id.to_owned());
        }
    }
    targets
}

pub(crate) fn refresh_account_serialized(
    state: &ServerState,
    account_id: &str,
) -> Result<Value, QuotaError> {
    let refresh_lock = state
        .backend
        .accounts
        .quota_refresh_locks
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .entry(account_id.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = refresh_lock
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    refresh_account_by_id(state, account_id)
}

pub(crate) fn sample_quotas_once(state: &Arc<ServerState>) -> QuotaSampleCounts {
    let targets = quota_sample_targets(state);
    if targets.is_empty() {
        return QuotaSampleCounts::default();
    }
    let worker_count = 4.min(targets.len());
    let counts = Arc::new(Mutex::new(QuotaSampleCounts::default()));
    let mut workers = Vec::with_capacity(worker_count);
    for offset in 0..worker_count {
        let state = Arc::clone(state);
        let counts = Arc::clone(&counts);
        let batch = targets
            .iter()
            .skip(offset)
            .step_by(worker_count)
            .cloned()
            .collect::<Vec<_>>();
        if let Ok(worker) = thread::Builder::new()
            .name("emp-quota-refresh".to_owned())
            .spawn(move || {
                for account_id in batch {
                    if state.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let sampled = refresh_account_serialized(&state, &account_id).is_ok();
                    if let Ok(mut counts) = counts.lock() {
                        if sampled {
                            counts.sampled += 1;
                        } else {
                            counts.failed += 1;
                        }
                    }
                }
            })
        {
            workers.push(worker);
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    counts
        .lock()
        .map_or_else(|_| QuotaSampleCounts::default(), |counts| *counts)
}
