pub(crate) mod commands;
#[cfg(all(test, unix))]
mod locks_tests;
mod sampler;

pub(crate) use sampler::{run as run_sampler, sample_quotas_once};

use crate::app::ServerState;
use crate::services::accounts::AccountCredentials;
use crate::services::accounts::account_allows_refresh;
use crate::services::accounts::account_public_snapshot;
use crate::services::accounts::native_account_snapshot;
use crate::services::accounts::native_auth_document;
use crate::services::accounts::{
    notify_quota_update, quota_owner_key, quota_refresh_cancellation, quota_refresh_locks,
};
use crate::util::system_now;
use emp_codex::quota::QuotaControl;
use emp_codex::quota::QuotaError;
use emp_codex::quota::access_token_snapshot;
use emp_codex::quota::consume_native_quota_reset;
use emp_codex::quota::read_native_login_quota;
use emp_codex::quota::run_quota_query;
use emp_codex::quota::run_quota_query_persisting;
use emp_codex::quota::run_quota_reset;
use emp_codex::quota::run_quota_reset_persisting;
use emp_codex::quota_history::{QuotaHistoryError, QuotaHistoryStore};
use emp_state::same_account_auth;
use serde_json::Value;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub(crate) struct QuotaState {
    native: Mutex<Option<Value>>,
    history: QuotaHistoryStore,
    sampler_wait: Mutex<()>,
    sampler_condition: Condvar,
}

impl QuotaState {
    pub(crate) fn new(state_root: &Path) -> Self {
        Self {
            native: Mutex::new(None),
            history: QuotaHistoryStore::new(state_root.join("quota_history.sqlite3")),
            sampler_wait: Mutex::new(()),
            sampler_condition: Condvar::new(),
        }
    }

    pub(crate) fn with_native_quota<T>(&self, read: impl FnOnce(&Value) -> T) -> Option<T> {
        self.native.lock().ok()?.as_ref().map(read)
    }

    pub(crate) fn set_native_quota(&self, quota: Option<Value>) -> Result<(), QuotaError> {
        let mut current = self
            .native
            .lock()
            .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
        *current = quota;
        Ok(())
    }

    pub(crate) fn record_snapshot(
        &self,
        owner: &str,
        quota: &Value,
        observed_at: i64,
    ) -> Result<usize, QuotaHistoryError> {
        self.history.append_snapshot(owner, quota, observed_at)
    }

    pub(crate) fn history_for_period(
        &self,
        owner: &str,
        start: i64,
        end: i64,
    ) -> Result<Value, QuotaHistoryError> {
        self.history.query_period(owner, start, end)
    }

    pub(crate) fn history_for_range(
        &self,
        owner: &str,
        range_name: &str,
        now: i64,
    ) -> Result<Value, QuotaHistoryError> {
        self.history.query(owner, range_name, now)
    }
}

fn quota_control(state: &ServerState) -> QuotaControl<'_> {
    QuotaControl {
        timeout: Duration::from_secs(45),
        cancelled: Some(&state.lifecycle.shutdown),
        account_cancelled: None,
    }
}

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
        .edit()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    let mut updated = config.clone();
    let Some(account) = updated
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
    let unchanged = account.get("credential_status").and_then(Value::as_str) == Some(status)
        && quota.is_none_or(|quota| account.get("quota") == Some(quota));
    if unchanged {
        drop(config);
        return account_public_snapshot(state, account_id)
            .ok_or_else(|| QuotaError::new("account changed during quota refresh", "quota_error"));
    }
    account["credential_status"] = Value::String(status.to_owned());
    if let Some(quota) = quota {
        account["quota"] = quota.clone();
    }
    config
        .commit(&updated)
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    drop(config);
    account_public_snapshot(state, account_id)
        .ok_or_else(|| QuotaError::new("account changed during quota refresh", "quota_error"))
}

/// Adopt an imported account's legacy rows into its credential identity; no rows is also success.
/// Excludes `@native`, whose current login cannot identify historical logins.
pub(crate) fn adopt_legacy_quota_history(state: &ServerState, account_id: &str) -> bool {
    if account_id == "@native" {
        return false;
    }
    adopt_owned_legacy_quota_history(state, account_id, quota_owner_key(state, account_id))
}

fn adopt_owned_legacy_quota_history(
    state: &ServerState,
    account_id: &str,
    owner: Result<String, QuotaError>,
) -> bool {
    owner.is_ok_and(|owner| {
        state
            .backend
            .quota
            .history
            .adopt_legacy_key(account_id, &owner)
            .is_ok()
    })
}

/// Adopt or delete legacy rows under the configuration lock before freeing or reassigning an account ID.
/// Final validation and commit must hold that lock; on error, the caller must not reassign the ID.
pub(crate) fn settle_legacy_quota_history(
    state: &ServerState,
    config: &Value,
    account_id: &str,
) -> Result<(), QuotaHistoryError> {
    if account_id == "@native"
        || adopt_owned_legacy_quota_history(
            state,
            account_id,
            crate::services::accounts::quota_owner_key_in(state, config, account_id),
        )
    {
        return Ok(());
    }
    state.backend.quota.history.delete_account(account_id)
}

/// Adopt every imported account's legacy rows, retaining rows with unknown identity
/// until attribution succeeds or [`settle_legacy_quota_history`] runs.
pub(crate) fn migrate_legacy_quota_history(state: &ServerState) {
    let accounts = state
        .backend
        .configuration
        .read()
        .ok()
        .and_then(|config| config.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    for account in accounts {
        if let Some(id) = account.get("id").and_then(Value::as_str) {
            adopt_legacy_quota_history(state, id);
        }
    }
}

fn record_quota_snapshot(state: &ServerState, account_id: &str, quota: &Value) {
    let result = quota_owner_key(state, account_id).ok().and_then(|owner| {
        state
            .backend
            .quota
            .record_snapshot(&owner, quota, system_now().trunc() as i64)
            .ok()
    });
    if result.is_none() {
        state.backend.diagnostics.journal.event("error", "quota_history_save_failed",
            &serde_json::json!({"account":state.backend.diagnostics.journal.pseudonym(account_id),"operation":"snapshot"}));
    }
}

fn refresh_imported_account(state: &ServerState, account_id: &str) -> Result<Value, QuotaError> {
    let shared = state.backend.team.shared_subscription(account_id);
    if shared
        && let Err(message) = state.backend.team.check_route(
            account_id,
            state.backend.modules.active(emp_modules::Module::Team),
            None,
        )
    {
        let error = message.public_error(state);
        let message = error.body["error"]["message"]
            .as_str()
            .expect("public error message");
        return Err(QuotaError::new(message, "quota_auth_required"));
    }
    let cancellation = quota_refresh_cancellation(state, account_id)
        .ok_or_else(|| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    let control = QuotaControl {
        account_cancelled: Some(&cancellation),
        ..quota_control(state)
    };
    if cancellation.load(std::sync::atomic::Ordering::Acquire) {
        return Err(QuotaError::new(
            "Quota check was cancelled",
            "quota_cancelled",
        ));
    }
    let credentials = AccountCredentials::for_account(state, account_id)?;
    let auth_file = credentials.auth_file();
    let allow_refresh = account_allows_refresh(state, account_id);
    let read_auth = || credentials.read();
    let query = |auth: &Value, refresh: bool| {
        let binary = crate::services::runtime::helper_binary(state)?;
        if !allow_refresh {
            return run_quota_query(&access_token_snapshot(auth), &binary, control, false)
                .map(|result| result.quota);
        }
        run_quota_query_persisting(auth, &binary, control, refresh, |refreshed| {
            credentials.persist(refreshed)
        })
    };
    let auth = read_auth()?;
    if native_auth_document(&state.backend.codex.auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return match read_native_login_quota(
            &state.backend.codex.auth_path,
            &crate::services::runtime::helper_binary(state)?,
            control,
        ) {
            Ok(quota) => {
                let _ = state.backend.quota.set_native_quota(Some(quota.clone()));
                save_account_quota_state(state, account_id, auth_file, "valid", Some(&quota))
                    .inspect(|_| record_quota_snapshot(state, account_id, &quota))
            }
            Err(error) => {
                if error.code() == "quota_auth_required" {
                    let _ = save_account_quota_state(state, account_id, auth_file, "invalid", None);
                }
                Err(error)
            }
        };
    }
    let quota = match query(&auth, false) {
        Ok(quota) => quota,
        Err(error) if !allow_refresh && error.code() == "quota_auth_required" => {
            let _ = save_account_quota_state(state, account_id, auth_file, "invalid", None);
            return Err(error);
        }
        Err(error)
            if allow_refresh
                && (error.code() == "quota_auth_required"
                    || error.should_retry_imported_refresh()) =>
        {
            let refreshed = read_auth()?;
            match query(&refreshed, true) {
                Ok(quota) => quota,
                Err(error) => {
                    if error.code() == "quota_auth_required" {
                        let _ =
                            save_account_quota_state(state, account_id, auth_file, "invalid", None);
                    }
                    return Err(error);
                }
            }
        }
        Err(error) => return Err(error),
    };
    save_account_quota_state(state, account_id, auth_file, "valid", Some(&quota))
        .inspect(|_| record_quota_snapshot(state, account_id, &quota))
}

fn replacement_account(state: &ServerState, account_id: &str) -> Option<String> {
    if account_id == "@native" {
        return None;
    }
    let config = state.backend.configuration.read().ok()?;
    let accounts = config["accounts"].as_array()?;
    let account = accounts
        .iter()
        .find(|account| account["id"] == account_id)?;
    state
        .backend
        .team
        .replacement_subscription(state, accounts, account)?["id"]
        .as_str()
        .map(str::to_owned)
}

fn with_refresh_target<T>(
    state: &ServerState,
    account_id: &str,
    refresh: impl FnOnce(Option<&str>) -> T,
) -> Option<T> {
    loop {
        let replacement = replacement_account(state, account_id);
        let locks = quota_refresh_locks(
            state,
            std::iter::once(account_id).chain(replacement.as_deref()),
        )?;
        let _guards = locks
            .iter()
            .map(|lock| lock.lock().ok())
            .collect::<Option<Vec<_>>>()?;
        if replacement_account(state, account_id) == replacement {
            return Some(refresh(replacement.as_deref()));
        }
    }
}

fn refresh_locked_account_inner(
    state: &ServerState,
    account_id: &str,
    replacement: Option<&str>,
) -> Result<Value, QuotaError> {
    if let Some(source) = replacement {
        refresh_locked_account(state, source, None)?;
        return account_public_snapshot(state, account_id)
            .ok_or_else(|| QuotaError::new("account changed during quota refresh", "quota_error"));
    }
    if account_id != "@native" {
        return refresh_imported_account(state, account_id);
    }
    let quota = read_native_login_quota(
        &state.backend.codex.auth_path,
        &crate::services::runtime::helper_binary(state)?,
        quota_control(state),
    )?;
    state.backend.quota.set_native_quota(Some(quota.clone()))?;
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .clone();
    record_quota_snapshot(state, account_id, &quota);
    Ok(native_account_snapshot(state, &config))
}

fn refresh_locked_account(
    state: &ServerState,
    account_id: &str,
    replacement: Option<&str>,
) -> Result<Value, QuotaError> {
    let started = std::time::Instant::now();
    let result = refresh_locked_account_inner(state, account_id, replacement);
    let journal = &state.backend.diagnostics.journal;
    journal.event(
        if result.is_ok() { "info" } else { "warning" },
        "quota_refresh",
        &serde_json::json!({
            "account": journal.pseudonym(account_id),
            "success": result.is_ok(),
            "error_class": result.as_ref().err().map(|error| error.code()),
            "duration_ms": started.elapsed().as_millis() as u64,
            "diagnostics": result.as_ref().err().map(|error| error.diagnostics()),
        }),
    );
    if let Err(error) = &result {
        let saved = quota_owner_key(state, account_id).ok().and_then(|owner| {
            state
                .backend
                .quota
                .history
                .append_failure(&owner, &error.diagnostics(), system_now().trunc() as i64)
                .ok()
        });
        if saved.is_none() {
            journal.event(
                "error",
                "quota_history_save_failed",
                &serde_json::json!({"account":journal.pseudonym(account_id),"operation":"failure"}),
            );
        }
    }
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
    period: Option<(i64, i64)>,
    now: i64,
) -> Result<Value, QuotaHistoryResponseError> {
    let owner = quota_owner_key(state, account_id).map_err(QuotaHistoryResponseError::Account)?;
    let quota = &state.backend.quota;
    let mut result = match period {
        Some((start, end)) => quota.history_for_period(&owner, start, end),
        None => quota.history_for_range(&owner, range_name, now),
    }
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
    credit_id: Option<&str>,
) -> Result<String, QuotaError> {
    if account_id == "@native" {
        return consume_native_quota_reset(
            &state.backend.codex.auth_path,
            &crate::services::runtime::helper_binary(state)?,
            Duration::from_secs(45),
            idempotency_key,
            credit_id,
        );
    }
    let credentials = AccountCredentials::for_account(state, account_id)?;
    let allow_refresh = account_allows_refresh(state, account_id);
    let read_auth = || credentials.read();
    let auth = read_auth()?;
    if native_auth_document(&state.backend.codex.auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return consume_native_quota_reset(
            &state.backend.codex.auth_path,
            &crate::services::runtime::helper_binary(state)?,
            Duration::from_secs(45),
            idempotency_key,
            credit_id,
        )
        .inspect_err(|error| {
            if !allow_refresh && error.code() == "quota_auth_required" {
                let _ = save_account_quota_state(
                    state,
                    account_id,
                    credentials.auth_file(),
                    "invalid",
                    None,
                );
            }
        });
    }
    let query = |auth: &Value, refresh: bool| {
        let binary = crate::services::runtime::helper_binary(state)?;
        if !allow_refresh {
            return run_quota_reset(
                &access_token_snapshot(auth),
                &binary,
                Duration::from_secs(45),
                false,
                idempotency_key,
                credit_id,
            );
        }
        run_quota_reset_persisting(
            auth,
            &binary,
            Duration::from_secs(45),
            refresh,
            idempotency_key,
            credit_id,
            |refreshed| credentials.persist(refreshed),
        )
    };
    let result = match query(&auth, false) {
        Ok(outcome) => Ok(outcome),
        Err(error) if allow_refresh && error.code() == "quota_auth_required" => {
            query(&read_auth()?, true)
        }
        Err(error) => Err(error),
    };
    if !allow_refresh
        && result
            .as_ref()
            .is_err_and(|error| error.code() == "quota_auth_required")
    {
        let _ =
            save_account_quota_state(state, account_id, credentials.auth_file(), "invalid", None);
    }
    result
}

pub(crate) fn refresh_account_serialized(
    state: &ServerState,
    account_id: &str,
) -> Result<Value, QuotaError> {
    with_refresh_target(state, account_id, |replacement| {
        refresh_locked_account(state, account_id, replacement)
    })
    .ok_or_else(|| QuotaError::new("Codex account quota check failed", "quota_error"))?
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QuotaSampleCounts {
    pub(crate) sampled: usize,
    pub(crate) failed: usize,
}
