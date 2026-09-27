//! Local management sessions and proxy caller authentication.

use crate::app::ServerState;
use crate::http::request::Request;
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

/// Single-use bootstrap tokens expire if the printed URL is never opened.
pub(crate) const BOOTSTRAP_LIFETIME_SECONDS: f64 = 10.0 * 60.0;

/// Export confirmations are issued immediately before a secret-bearing export.
pub(crate) const EXPORT_CONFIRMATION_LIFETIME_SECONDS: f64 = 60.0;
pub(crate) const EXPORT_CONFIRMATION_OPERATION: &str = "/api/migration/export";

/// Management requests carry the origin-scoped session token in this header.
pub(crate) const SESSION_HEADER: &str = "X-EMP-Session";

/// The Web UI presents the single-use bootstrap token in this header.
pub(crate) const BOOTSTRAP_HEADER: &str = "X-EMP-Bootstrap";

/// Browsers send cookies to every port on the same host, so management no
/// longer uses one. Responses that serve the UI expire any legacy cookie.
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

    /// Replace the persisted session after a successful bootstrap, so every
    /// browser login receives a fresh token and earlier ones stop working.
    pub(crate) fn rotate(&self, now: f64) -> Option<(String, u64)> {
        let mut session = self.session.lock().ok()?;
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        let rotated = load_or_create_web_session(&self.path, now).ok()?;
        if !rotated.is_active_at(now) {
            return None;
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

    /// Consume the pending confirmation. Any presented value, correct or not,
    /// spends it so a guessed or replayed value never gets a second attempt.
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

pub(crate) fn session_header_value(request: Request<'_>) -> Option<String> {
    if request.header_count(SESSION_HEADER) != 1 {
        return None;
    }
    let value = request.header(SESSION_HEADER)?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| value.to_owned())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

pub(crate) fn same_origin(request: Request<'_>, port: u16) -> bool {
    let allowed_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if request.header_count("Host") != 1 || request.header_count("Origin") > 1 {
        return false;
    }
    let Some(host) = request.header("Host") else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if !allowed_hosts.iter().any(|allowed| allowed == host) {
        return false;
    }

    let Some(origin) = request.header("Origin") else {
        return true;
    };
    match parse_origin(origin, port) {
        Some(true) => true,
        Some(false) | None => false,
    }
}

fn parse_origin(origin: &str, port: u16) -> Option<bool> {
    let (scheme, rest) = origin.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Some(false);
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Some(false);
    }
    let (host, origin_port) = if let Some(host) = authority.strip_prefix('[') {
        let Some((host, suffix)) = host.split_once(']') else {
            return Some(false);
        };
        let port = suffix.strip_prefix(':')?;
        (host, port)
    } else {
        let (host, origin_port) = authority.rsplit_once(':')?;
        (host, origin_port)
    };
    let host = host.to_ascii_lowercase();
    let origin_port = origin_port.parse::<u16>().ok()?;
    Some((host == "127.0.0.1" || host == "localhost") && origin_port == port)
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
    tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
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

pub(crate) fn proxy_allowed(request: Request<'_>, state: &ServerState, now: f64) -> bool {
    if !same_origin(request, state.port) {
        return false;
    }
    if request.header_count("Authorization") > 1 {
        return false;
    }
    let supplied_session = request.session_token();
    state.sessions.contains(supplied_session.as_deref(), now)
        || valid_caller_authorization(
            request.header("Authorization"),
            &state.backend.accounts.native_auth_path,
        )
}
