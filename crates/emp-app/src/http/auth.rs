pub(crate) use crate::server::http::management::{same_origin, session_header_value};
pub(crate) use crate::server::http::proxy::proxy_identity;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_state::WebSession;
use emp_state::load_or_create_web_session;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

pub(crate) const BOOTSTRAP_LIFETIME_SECONDS: f64 = 10.0 * 60.0;

/// Export confirmations are issued immediately before a secret-bearing export.
pub(crate) const EXPORT_CONFIRMATION_LIFETIME_SECONDS: f64 = 60.0;
pub(crate) const EXPORT_CONFIRMATION_OPERATION: &str = "/api/migration/export";

pub(crate) const SESSION_HEADER: &str = "X-EMP-Session";

pub(crate) const BOOTSTRAP_HEADER: &str = "X-EMP-Bootstrap";

/// UI responses expire the legacy cookie; management sessions use origin-scoped headers.
pub(crate) const CLEAR_LEGACY_SESSION_COOKIE: &str =
    "emp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0";

pub(crate) struct BootstrapToken {
    pub(crate) token: String,
    pub(crate) used: AtomicBool,
    pub(crate) expires_at: f64,
}

impl BootstrapToken {
    pub(crate) fn matches(&self, supplied: &str, now: f64) -> bool {
        let matched = constant_time_eq(supplied.as_bytes(), self.token.as_bytes());
        matched && now.is_finite() && now < self.expires_at
    }

    pub(crate) fn consume(&self) -> bool {
        self.used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Releases a consumed token after session creation fails so the sign-in link can be retried.
    pub(crate) fn release(&self) {
        self.used.store(false, Ordering::Release);
    }
}

struct ExportConfirmation {
    token: String,
    session_token: String,
    operation: &'static str,
    expires_at: f64,
}

pub(crate) struct SessionStore {
    pub(crate) session: Mutex<WebSession>,
    pub(crate) path: PathBuf,
    export_confirmation: Mutex<Option<ExportConfirmation>>,
}

impl SessionStore {
    pub(crate) fn new(session: WebSession, path: PathBuf) -> Self {
        Self {
            session: Mutex::new(session),
            path,
            export_confirmation: Mutex::new(None),
        }
    }

    pub(crate) fn contains(&self, supplied: Option<&str>, now: f64) -> bool {
        let Some(supplied) = supplied else {
            return false;
        };
        let Ok(session) = self.session.lock() else {
            return false;
        };
        session.matches_at(supplied, now)
    }

    /// A successful bootstrap persists a fresh token and invalidates earlier browser sessions.
    pub(crate) fn rotate(&self, now: f64) -> Option<(String, u64)> {
        let mut session = self.session.lock().ok()?;
        let backup = self.path.with_extension("rotating");
        let had_previous = match std::fs::rename(&self.path, &backup) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return None,
        };
        let rotated = match load_or_create_web_session(&self.path, now) {
            Ok(rotated) if rotated.is_active_at(now) => rotated,
            _ => {
                if had_previous {
                    let _ = std::fs::rename(&backup, &self.path);
                }
                return None;
            }
        };
        if had_previous {
            let _ = std::fs::remove_file(&backup);
        }
        *session = rotated;
        if let Ok(mut confirmation) = self.export_confirmation.lock() {
            *confirmation = None;
        }
        Some((
            session.token().to_owned(),
            session.remaining_seconds_at(now),
        ))
    }

    pub(crate) fn issue_export_confirmation(
        &self,
        operation: &'static str,
        now: f64,
    ) -> Option<String> {
        if !now.is_finite() {
            return None;
        }
        let session = self.session.lock().ok()?;
        if !session.is_active_at(now) {
            return None;
        }
        let mut random = [0_u8; 32];
        getrandom::getrandom(&mut random).ok()?;
        let token = URL_SAFE_NO_PAD.encode(random);
        let mut confirmation = self.export_confirmation.lock().ok()?;
        *confirmation = Some(ExportConfirmation {
            token: token.clone(),
            session_token: session.token().to_owned(),
            operation,
            expires_at: now + EXPORT_CONFIRMATION_LIFETIME_SECONDS,
        });
        Some(token)
    }

    /// Any presented value consumes the confirmation, including an incorrect guess or replay.
    pub(crate) fn consume_export_confirmation(
        &self,
        supplied: &str,
        operation: &str,
        now: f64,
    ) -> bool {
        if !now.is_finite() {
            return false;
        }
        let Ok(session) = self.session.lock() else {
            return false;
        };
        if !session.is_active_at(now) {
            return false;
        }
        let Ok(mut confirmation) = self.export_confirmation.lock() else {
            return false;
        };
        let Some(pending) = confirmation.take() else {
            return false;
        };
        let token_matches = constant_time_eq(supplied.as_bytes(), pending.token.as_bytes());
        let session_matches =
            constant_time_eq(session.token().as_bytes(), pending.session_token.as_bytes());
        token_matches
            && session_matches
            && operation == pending.operation
            && now < pending.expires_at
    }
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

pub(crate) const MAX_NATIVE_AUTH_BYTES: usize = 1024 * 1024;

pub(crate) fn codex_auth_path() -> PathBuf {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".codex"))
        })
        .unwrap_or_else(|| PathBuf::from(".codex"));
    emp_state::config::resolve_user_path(&home).join("auth.json")
}

fn native_access_token(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    if raw.len() > MAX_NATIVE_AUTH_BYTES {
        return None;
    }
    let auth: Value = serde_json::from_slice(&raw).ok()?;
    let auth = auth.as_object()?;
    let tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .unwrap_or(auth);
    [tokens.get("access_token"), auth.get("OPENAI_API_KEY")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

pub(crate) fn valid_caller_authorization(value: Option<&str>, auth_path: &Path) -> bool {
    let Some(supplied) = value
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    native_access_token(auth_path)
        .is_some_and(|candidate| constant_time_eq(supplied.as_bytes(), candidate.as_bytes()))
}
