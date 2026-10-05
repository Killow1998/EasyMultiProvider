//! Account-scoped quota refresh, reset and history operations.
pub(crate) mod commands;

use crate::app::ServerState;
use crate::services::accounts::CredentialOperation;
use crate::services::accounts::account_public_snapshot;
use crate::services::accounts::native_account_snapshot;
use crate::services::accounts::native_auth_document;
use crate::services::accounts::regular_file;
use crate::services::accounts::{
    duplicate_accounts, notify_quota_update, quota_owner_key, quota_refresh_lock,
};
use crate::util::system_now;
use emp_codex::quota::QuotaControl;
use emp_codex::quota::QuotaError;
use emp_codex::quota::consume_native_quota_reset;
use emp_codex::quota::read_native_login_quota;
use emp_codex::quota::run_quota_query_persisting;
use emp_codex::quota::run_quota_reset_persisting;
use emp_codex::quota_history::QuotaHistoryError;
use emp_state::same_account_auth;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

fn quota_control(state: &ServerState) -> QuotaControl<'_> {
    QuotaControl {
        timeout: Duration::from_secs(45),
        cancelled: Some(&state.shutdown),
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
    // The sampler runs every 44 s; leave config.json alone when nothing changed.
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

/// Durable-save attempts for credentials Codex rotated during a quota check.
const PERSIST_ROTATION_ATTEMPTS: u32 = 3;

/// Test-only fault injection: rotated-credential saves to these auth files
/// fail. A set, so tests running in parallel do not clear each other's paths.
#[cfg(test)]
pub(crate) static FAIL_ROTATION_SAVES_TO: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

fn save_rotated_credential(
    vault: &emp_state::VaultStore,
    auth_path: &Path,
    auth: &Value,
) -> Result<(), ()> {
    #[cfg(test)]
    if FAIL_ROTATION_SAVES_TO.lock().is_ok_and(|failing| {
        auth_path
            .to_str()
            .is_some_and(|path| failing.contains(path))
    }) {
        return Err(());
    }
    vault.write_encrypted_json(auth_path, auth).map_err(|_| ())
}

/// The stored credentials of one imported account, as quota refresh and
/// reset both read and rotate them.
struct AccountCredentials<'a> {
    state: &'a ServerState,
    auth_file: String,
    /// Keeps shutdown from its final save until a rotation here is saved or
    /// pending; `None` for the saves themselves.
    _operation: Option<CredentialOperation<'a>>,
}

impl<'a> AccountCredentials<'a> {
    fn for_account(state: &'a ServerState, account_id: &str) -> Result<Self, QuotaError> {
        let operation = state
            .backend
            .accounts
            .credential_operations
            .enter()
            .ok_or_else(|| QuotaError::new("EMP is shutting down", "quota_error"))?;
        let target = state
            .backend
            .configuration
            .read()
            .ok()
            .and_then(|config| {
                config
                    .get("accounts")?
                    .as_array()?
                    .iter()
                    .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
                    .cloned()
            })
            .ok_or_else(|| {
                QuotaError::new(format!("unknown account: {account_id}"), "quota_error")
            })?;
        let auth_file = target
            .get("auth_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                QuotaError::new("account credentials are not configured", "quota_error")
            })?
            .to_owned();
        Ok(Self {
            state,
            auth_file,
            _operation: Some(operation),
        })
    }

    fn auth_path(&self) -> &Path {
        Path::new(&self.auth_file)
    }

    fn vault(&self) -> &emp_state::VaultStore {
        &self.state.backend.configuration.vault
    }

    fn pending(&self) -> &Mutex<std::collections::BTreeMap<String, Value>> {
        &self.state.backend.accounts.pending_rotations
    }

    /// Current credential: a rotated credential whose save failed supersedes
    /// the stored copy, whose refresh token Codex may already have
    /// invalidated upstream.
    fn read(&self) -> Result<Value, QuotaError> {
        let rotated = self
            .pending()
            .lock()
            .ok()
            .and_then(|pending| pending.get(&self.auth_file).cloned());
        if let Some(rotated) = rotated {
            if save_rotated_credential(self.vault(), self.auth_path(), &rotated).is_ok()
                && let Ok(mut pending) = self.pending().lock()
            {
                pending.remove(&self.auth_file);
            }
            return Ok(rotated);
        }
        self.vault()
            .read_encrypted_json(self.auth_path())
            .map_err(|_| QuotaError::new("stored encrypted auth.json is invalid", "quota_error"))
    }

    /// Durably save a credential Codex rotated. On failure the credential is
    /// kept in memory: the next check uses it, and the quota sampler and
    /// shutdown retry the save ([`flush_pending_rotations`]).
    fn persist(&self, refreshed: &Value) -> Result<(), ()> {
        for attempt in 0..PERSIST_ROTATION_ATTEMPTS {
            if save_rotated_credential(self.vault(), self.auth_path(), refreshed).is_ok() {
                if let Ok(mut pending) = self.pending().lock() {
                    pending.remove(&self.auth_file);
                }
                return Ok(());
            }
            if attempt + 1 < PERSIST_ROTATION_ATTEMPTS {
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
        }
        if let Ok(mut pending) = self.pending().lock() {
            pending.insert(self.auth_file.clone(), refreshed.clone());
        }
        Err(())
    }
}

/// Retry the durable save of every rotated credential still held in memory.
/// Each save runs under its account's refresh lock, so it cannot overwrite a
/// newer rotation a concurrent check persisted. Credentials of accounts that
/// are gone are dropped. Returns the number still unsaved.
pub(crate) fn flush_pending_rotations(state: &ServerState) -> usize {
    let pending_files = state
        .backend
        .accounts
        .pending_rotations
        .lock()
        .map(|pending| pending.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if pending_files.is_empty() {
        return 0;
    }
    let owners = state
        .backend
        .configuration
        .read()
        .ok()
        .and_then(|config| config.get("accounts").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|account| {
            Some((
                account.get("auth_file")?.as_str()?.to_owned(),
                account.get("id")?.as_str()?.to_owned(),
            ))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut unsaved = 0;
    for auth_file in pending_files {
        let Some(account_id) = owners.get(&auth_file) else {
            crate::services::accounts::forget_pending_rotation(state, Path::new(&auth_file));
            continue;
        };
        let Some(lock) = quota_refresh_lock(state, account_id) else {
            unsaved += 1;
            continue;
        };
        let Ok(_guard) = lock.lock() else {
            unsaved += 1;
            continue;
        };
        let credentials = AccountCredentials {
            state,
            auth_file,
            _operation: None,
        };
        let rotated = credentials
            .pending()
            .lock()
            .ok()
            .and_then(|pending| pending.get(&credentials.auth_file).cloned());
        if let Some(rotated) = rotated
            && credentials.persist(&rotated).is_err()
        {
            unsaved += 1;
        }
    }
    unsaved
}

/// Attach history recorded under an account's legacy local id key to the
/// upstream identity of its current credentials. Before identity keys, EMP
/// deleted an account's history with the account and recorded an import that
/// duplicated another account under that account's key, so rows under an
/// imported account's id were recorded from that entry's current
/// credentials. Returns whether the rows now belong to an identity (or there
/// were none to move).
///
/// `@native` rows are never adopted: they were recorded from whichever login
/// Codex held at the time, and the current login cannot prove it recorded
/// them, so they stay under their legacy key.
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
            .accounts
            .quota_history
            .adopt_legacy_key(account_id, &owner)
            .is_ok()
    })
}

/// Settle an account id's legacy rows before the id is freed or reassigned
/// to other credentials: adopt them into the current identity, or delete
/// them as pre-identity EMP did, so a later account reusing the id cannot
/// inherit them. Callers must not free or reassign the id on error, and run
/// it under the configuration lock (`config` is the locked configuration)
/// together with their final validation and commit, so a request rejected
/// later cannot have settled the rows.
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
    state
        .backend
        .accounts
        .quota_history
        .delete_account(account_id)
}

/// Run [`adopt_legacy_quota_history`] for every imported account. Accounts
/// whose identity cannot be derived yet keep their rows until they can, or
/// until [`settle_legacy_quota_history`] runs for them.
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
    let credentials = AccountCredentials::for_account(state, account_id)?;
    let auth_file = credentials.auth_file.clone();
    let read_auth = || credentials.read();
    let query = |auth: &Value, allow_refresh: bool| {
        run_quota_query_persisting(
            auth,
            &crate::services::runtime::helper_binary(state)?,
            quota_control(state),
            allow_refresh,
            |refreshed| credentials.persist(refreshed),
        )
    };
    let auth = read_auth()?;
    if native_auth_document(&state.backend.accounts.native_auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return match read_native_login_quota(
            &state.backend.accounts.native_auth_path,
            &crate::services::runtime::helper_binary(state)?,
            quota_control(state),
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
        Err(error)
            if error.code() == "quota_auth_required" || error.should_retry_imported_refresh() =>
        {
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
        &crate::services::runtime::helper_binary(state)?,
        quota_control(state),
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
        .read()
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
    let journal = &state.backend.diagnostics.journal;
    journal.event(
        if result.is_ok() { "info" } else { "warning" },
        "quota_refresh",
        &serde_json::json!({
            "account": journal.pseudonym(account_id),
            "success": result.is_ok(),
            "error_class": result.as_ref().err().map(|error| error.code()),
        }),
    );
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
    let history = &state.backend.accounts.quota_history;
    let mut result = match period {
        Some((start, end)) => history.query_period(&owner, start, end),
        None => history.query(&owner, range_name, now),
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
            &state.backend.accounts.native_auth_path,
            &crate::services::runtime::helper_binary(state)?,
            Duration::from_secs(45),
            idempotency_key,
            credit_id,
        );
    }
    let credentials = AccountCredentials::for_account(state, account_id)?;
    let read_auth = || credentials.read();
    let auth = read_auth()?;
    if native_auth_document(&state.backend.accounts.native_auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return consume_native_quota_reset(
            &state.backend.accounts.native_auth_path,
            &crate::services::runtime::helper_binary(state)?,
            Duration::from_secs(45),
            idempotency_key,
            credit_id,
        );
    }
    let query = |auth: &Value, allow_refresh: bool| {
        run_quota_reset_persisting(
            auth,
            &crate::services::runtime::helper_binary(state)?,
            Duration::from_secs(45),
            allow_refresh,
            idempotency_key,
            credit_id,
            |refreshed| credentials.persist(refreshed),
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
    let Some(config) = state
        .backend
        .configuration
        .read()
        .ok()
        .map(|config| config.clone())
    else {
        return targets;
    };
    // Sample each upstream account once: an import of the native login or of
    // another imported account shares its quota and history owner.
    let duplicates = duplicate_accounts(
        &config,
        &state.backend.configuration.vault,
        &state.backend.accounts.native_auth_path,
    );
    let accounts = config
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
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
        if !duplicates.contains_key(account_id) {
            targets.push(account_id.to_owned());
        }
    }
    targets
}

pub(crate) fn refresh_account_serialized(
    state: &ServerState,
    account_id: &str,
) -> Result<Value, QuotaError> {
    let refresh_lock = quota_refresh_lock(state, account_id)
        .ok_or_else(|| QuotaError::new("Codex account quota check failed", "quota_error"))?;
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
