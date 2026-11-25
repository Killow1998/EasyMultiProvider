use super::{AccountState, quota_refresh_lock};
use crate::app::ServerState;
use emp_codex::quota::QuotaError;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const PERSIST_ROTATION_ATTEMPTS: u32 = 3;

/// Test-only fault injection: rotated-credential saves to these auth files
/// fail. A set, so tests running in parallel do not clear each other's paths.
#[cfg(all(test, unix))]
static FAIL_ROTATION_SAVES_TO: Mutex<std::collections::BTreeSet<String>> =
    Mutex::new(std::collections::BTreeSet::new());

fn save_rotated_credential(
    vault: &emp_state::VaultStore,
    auth_path: &Path,
    auth: &Value,
) -> Result<(), ()> {
    if AccountState::rotation_save_fails_for(auth_path) {
        return Err(());
    }
    vault.write_encrypted_json(auth_path, auth).map_err(|_| ())
}

#[cfg(all(test, unix))]
impl AccountState {
    fn rotation_save_fails_for(auth_path: &Path) -> bool {
        FAIL_ROTATION_SAVES_TO.lock().is_ok_and(|failing| {
            auth_path
                .to_str()
                .is_some_and(|path| failing.contains(path))
        })
    }

    pub(crate) fn test_fail_rotation_saves_to(auth_file: &str, failing: bool) {
        let mut paths = FAIL_ROTATION_SAVES_TO.lock().unwrap();
        if failing {
            paths.insert(auth_file.to_owned());
        } else {
            paths.remove(auth_file);
        }
    }

    pub(crate) fn test_pending_rotation(&self, auth_file: &str) -> Option<Value> {
        self.pending_rotation(auth_file)
    }

    pub(crate) fn test_pending_rotations_empty(&self) -> bool {
        self.pending_rotation_paths().unwrap().is_empty()
    }

    pub(crate) fn test_retain_pending_rotation(&self, auth_file: &str, auth: &Value) {
        self.retain_pending_rotation(auth_file, auth);
    }
}

#[cfg(not(all(test, unix)))]
impl AccountState {
    fn rotation_save_fails_for(_: &Path) -> bool {
        false
    }
}

impl AccountState {
    pub(crate) fn close_credential_operations(&self, timeout: Duration) -> usize {
        self.credential_operations.close_and_drain(timeout)
    }

    pub(super) fn forget_pending_rotation(&self, auth_path: &Path) {
        if let Ok(mut pending) = self.pending_rotations.lock() {
            pending.remove(auth_path.to_string_lossy().as_ref());
        }
    }

    pub(super) fn pending_rotation_paths(&self) -> Result<Vec<String>, ()> {
        self.pending_rotations
            .lock()
            .map(|pending| pending.keys().cloned().collect())
            .map_err(|_| ())
    }

    fn pending_rotation(&self, auth_file: &str) -> Option<Value> {
        self.pending_rotations.lock().ok()?.get(auth_file).cloned()
    }

    fn retain_pending_rotation(&self, auth_file: &str, auth: &Value) {
        if let Ok(mut pending) = self.pending_rotations.lock() {
            pending.insert(auth_file.to_owned(), auth.clone());
        }
    }
}

pub(crate) struct AccountCredentials<'a> {
    state: &'a ServerState,
    auth_file: String,
    /// Keeps shutdown from its final save until a rotation here is saved or
    /// pending; `None` for the saves themselves.
    _operation: Option<CredentialOperation<'a>>,
}

impl<'a> AccountCredentials<'a> {
    pub(crate) fn for_account(
        state: &'a ServerState,
        account_id: &str,
    ) -> Result<Self, QuotaError> {
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

    pub(crate) fn auth_file(&self) -> &str {
        &self.auth_file
    }

    fn auth_path(&self) -> &Path {
        Path::new(&self.auth_file)
    }

    fn vault(&self) -> &emp_state::VaultStore {
        &self.state.backend.configuration.vault
    }

    /// Prefer unsaved rotated credentials: Codex may have invalidated the stored refresh token.
    pub(crate) fn read(&self) -> Result<Value, QuotaError> {
        let accounts = &self.state.backend.accounts;
        if let Some(rotated) = accounts.pending_rotation(&self.auth_file) {
            if save_rotated_credential(self.vault(), self.auth_path(), &rotated).is_ok() {
                accounts.forget_pending_rotation(self.auth_path());
            }
            return Ok(rotated);
        }
        self.vault()
            .read_encrypted_json(self.auth_path())
            .map_err(|_| QuotaError::new("stored encrypted auth.json is invalid", "quota_error"))
    }

    /// Save rotated credentials durably or retain them for the next check, with retries
    /// by the quota sampler and shutdown via [`flush_pending_rotations`].
    pub(crate) fn persist(&self, refreshed: &Value) -> Result<(), ()> {
        let accounts = &self.state.backend.accounts;
        for attempt in 0..PERSIST_ROTATION_ATTEMPTS {
            if save_rotated_credential(self.vault(), self.auth_path(), refreshed).is_ok() {
                accounts.forget_pending_rotation(self.auth_path());
                return Ok(());
            }
            if attempt + 1 < PERSIST_ROTATION_ATTEMPTS {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "轮换凭据持久化失败后按既有指数退避间隔重试"
                )]
                std::thread::sleep(Duration::from_millis(100 << attempt));
            }
        }
        accounts.retain_pending_rotation(&self.auth_file, refreshed);
        Err(())
    }
}

/// Retry pending saves under each account's refresh lock to avoid overwriting newer
/// rotations; drop credentials of removed accounts and return the number still unsaved.
pub(crate) fn flush_pending_rotations(state: &ServerState) -> usize {
    let accounts = &state.backend.accounts;
    let pending_files = accounts.pending_rotation_paths().unwrap_or_default();
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
        .collect::<BTreeMap<_, _>>();
    let mut unsaved = 0;
    for auth_file in pending_files {
        let Some(account_id) = owners.get(&auth_file) else {
            accounts.forget_pending_rotation(Path::new(&auth_file));
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
        let rotated = accounts.pending_rotation(&credentials.auth_file);
        if let Some(rotated) = rotated
            && credentials.persist(&rotated).is_err()
        {
            unsaved += 1;
        }
    }
    unsaved
}

/// Counts in-flight credential operations and refuses new ones once closed.
#[derive(Default)]
pub(super) struct CredentialOperationGate {
    state: Mutex<(usize, bool)>,
    idle: Condvar,
}

struct CredentialOperation<'a>(&'a CredentialOperationGate);

impl CredentialOperationGate {
    /// `None` once the gate is closed for shutdown.
    fn enter(&self) -> Option<CredentialOperation<'_>> {
        let mut state = self.state.lock().ok()?;
        if state.1 {
            return None;
        }
        state.0 += 1;
        Some(CredentialOperation(self))
    }

    /// Refuse new operations, wait up to `timeout`, and return the number still running,
    /// whose rotated credentials may remain unsaved.
    fn close_and_drain(&self, timeout: Duration) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        state.1 = true;
        let deadline = Instant::now() + timeout;
        while state.0 > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
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
