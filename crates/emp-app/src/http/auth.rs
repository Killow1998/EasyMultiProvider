//! Local management sessions and proxy caller authentication.

use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::request::query_values;
use emp_state::WebSession;
use emp_state::load_or_create_web_session;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

pub(crate) struct BootstrapToken {
    pub(crate) token: String,
    pub(crate) used: AtomicBool,
}

impl BootstrapToken {
    pub(crate) fn matches(&self, supplied: &str) -> bool {
        constant_time_eq(supplied.as_bytes(), self.token.as_bytes())
    }

    pub(crate) fn consume(&self) -> bool {
        self.used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

pub(crate) struct SessionStore {
    pub(crate) session: Mutex<WebSession>,
    pub(crate) path: PathBuf,
}

impl SessionStore {
    pub(crate) fn contains(&self, supplied: Option<&str>, now: f64) -> bool {
        let Some(supplied) = supplied else {
            return false;
        };
        let Ok(session) = self.session.lock() else {
            return false;
        };
        session.matches_at(supplied, now)
    }

    pub(crate) fn refresh_header(&self, now: f64) -> Option<String> {
        let session = self.session.lock().ok()?;
        if !session.is_active_at(now) {
            return None;
        }
        Some(session_cookie(
            session.token(),
            session.remaining_seconds_at(now),
        ))
    }

    pub(crate) fn refresh_or_rotate_header(&self, now: f64) -> Option<String> {
        let mut session = self.session.lock().ok()?;
        if !session.is_active_at(now) {
            *session = load_or_create_web_session(&self.path, now).ok()?;
        }
        Some(session_cookie(
            session.token(),
            session.remaining_seconds_at(now),
        ))
    }
}

pub(crate) fn parse_session_cookie(header: &str) -> Option<String> {
    let mut result = None;
    for pair in header.split(';') {
        let (name, raw_value) = pair.trim().split_once('=')?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        {
            return None;
        }
        let raw_value = raw_value.trim();
        let value = if raw_value.starts_with('"') || raw_value.ends_with('"') {
            raw_value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))?
        } else {
            raw_value
        };
        if value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return None;
        }
        if name.eq_ignore_ascii_case("emp_session") {
            result = Some(value.to_owned());
        }
    }
    result
}

pub(crate) fn session_cookie(token: &str, max_age: u64) -> String {
    format!("emp_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}")
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

pub(crate) fn bootstrap_value(target: &str) -> Option<String> {
    let values = query_values(target, "bootstrap");
    (values.len() == 1 && values[0].is_ascii()).then(|| values[0].clone())
}

pub(crate) fn same_origin(request: Request<'_>, port: u16) -> bool {
    let allowed_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
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
    let supplied_cookie = request.session_cookie();
    state.sessions.contains(supplied_cookie.as_deref(), now)
        || valid_caller_authorization(
            request.header("Authorization"),
            &state.backend.accounts.native_auth_path,
        )
}
