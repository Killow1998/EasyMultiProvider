//! Persistent browser-management sessions compatible with the Python backend.

use crate::filesystem::{FilesystemError, atomic_write_config};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const WEB_SESSION_LIFETIME_SECONDS: f64 = 30.0 * 24.0 * 60.0 * 60.0;
pub const WEB_SESSION_TOKEN_BYTES: usize = 32;
pub const WEB_SESSION_TOKEN_LENGTH: usize = 43;

#[derive(Clone)]
pub struct WebSession {
    token: String,
    expires_at: f64,
}

impl WebSession {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn expires_at(&self) -> f64 {
        self.expires_at
    }

    pub fn is_active_at(&self, now: f64) -> bool {
        now.is_finite() && now < self.expires_at
    }

    pub fn remaining_seconds_at(&self, now: f64) -> u64 {
        if !now.is_finite() || now >= self.expires_at {
            return 0;
        }
        (self.expires_at - now).floor().max(0.0) as u64
    }

    /// Compare a browser token without returning early on token contents.
    pub fn matches_at(&self, supplied: &str, now: f64) -> bool {
        self.is_active_at(now) && constant_time_bytes_eq(supplied.as_bytes(), self.token.as_bytes())
    }
}

impl fmt::Debug for WebSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebSession")
            .field("token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSessionError {
    ClockInvalid,
    RandomUnavailable,
    StateUnavailable,
}

impl fmt::Display for WebSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClockInvalid => formatter.write_str("web session clock is invalid"),
            Self::RandomUnavailable => {
                formatter.write_str("secure randomness for the web session is unavailable")
            }
            Self::StateUnavailable => formatter.write_str("web session state is unavailable"),
        }
    }
}

impl std::error::Error for WebSessionError {}

#[derive(Deserialize, Serialize)]
struct StoredWebSession {
    token: String,
    expires_at: f64,
}

pub fn web_session_path(config_path: &Path) -> Result<PathBuf, WebSessionError> {
    let parent = config_path
        .parent()
        .ok_or(WebSessionError::StateUnavailable)?;
    Ok(parent.join("state").join("web-session.json"))
}

/// Read a still-valid Python/Rust session or atomically rotate it.
///
/// A saved session is accepted only within the same 30-day window as the
/// Python implementation. Invalid JSON and stale state rotate safely; other
/// filesystem read failures remain visible to the caller.
pub fn load_or_create_web_session(path: &Path, now: f64) -> Result<WebSession, WebSessionError> {
    if !now.is_finite() {
        return Err(WebSessionError::ClockInvalid);
    }

    match fs::read(path) {
        Ok(raw) => {
            if let Ok(saved) = serde_json::from_slice::<StoredWebSession>(&raw)
                && valid_saved_session(&saved, now)
            {
                return Ok(WebSession {
                    token: saved.token,
                    expires_at: saved.expires_at,
                });
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(WebSessionError::StateUnavailable),
    }

    let mut random = [0_u8; WEB_SESSION_TOKEN_BYTES];
    getrandom::getrandom(&mut random).map_err(|_| WebSessionError::RandomUnavailable)?;
    let session = WebSession {
        token: URL_SAFE_NO_PAD.encode(random),
        expires_at: now + WEB_SESSION_LIFETIME_SECONDS,
    };
    let encoded = serde_json::to_vec(&StoredWebSession {
        token: session.token.clone(),
        expires_at: session.expires_at,
    })
    .map_err(|_| WebSessionError::StateUnavailable)?;
    atomic_write_config(path, &encoded).map_err(map_filesystem_error)?;
    Ok(session)
}

fn valid_saved_session(saved: &StoredWebSession, now: f64) -> bool {
    saved.token.len() == WEB_SESSION_TOKEN_LENGTH
        && saved.token.is_ascii()
        && saved
            .token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        && saved.expires_at.is_finite()
        && now < saved.expires_at
        && saved.expires_at <= now + WEB_SESSION_LIFETIME_SECONDS
}

fn constant_time_bytes_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    let length = left.len().max(right.len());
    for index in 0..length {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
}

fn map_filesystem_error(_: FilesystemError) -> WebSessionError {
    WebSessionError::StateUnavailable
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const NOW: f64 = 1_790_000_000.25;

    #[test]
    fn creates_and_reuses_a_python_compatible_session() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory
            .path()
            .canonicalize()
            .expect("canonical temporary directory")
            .join("state/web-session.json");
        let created = load_or_create_web_session(&path, NOW).expect("create session");
        assert_eq!(created.token().len(), WEB_SESSION_TOKEN_LENGTH);
        assert!(
            created
                .token()
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        );
        assert_eq!(created.expires_at(), NOW + WEB_SESSION_LIFETIME_SECONDS);

        let reused = load_or_create_web_session(&path, NOW + 10.0).expect("reuse session");
        assert_eq!(reused.token(), created.token());
        assert_eq!(reused.expires_at(), created.expires_at());

        let stored: Value =
            serde_json::from_slice(&fs::read(path).expect("saved session")).expect("session JSON");
        assert_eq!(stored["token"], created.token());
        assert_eq!(stored["expires_at"], created.expires_at());
    }

    #[test]
    fn rotates_invalid_expired_and_too_distant_state() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory
            .path()
            .canonicalize()
            .expect("canonical temporary directory")
            .join("web-session.json");
        for payload in [
            b"not json".as_slice(),
            br#"{"token":"short","expires_at":1790000100}"#,
            br#"{"token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","expires_at":1789999999}"#,
            br#"{"token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","expires_at":1792592001}"#,
        ] {
            fs::write(&path, payload).expect("write invalid state");
            let session = load_or_create_web_session(&path, NOW).expect("rotate state");
            assert_ne!(
                session.token(),
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            );
            assert_eq!(session.expires_at(), NOW + WEB_SESSION_LIFETIME_SECONDS);
        }
    }

    #[test]
    fn validates_cookie_and_max_age_without_exposing_token_in_debug() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory
            .path()
            .canonicalize()
            .expect("canonical temporary directory")
            .join("web-session.json");
        let session = load_or_create_web_session(&path, NOW).expect("create session");
        assert!(session.matches_at(session.token(), NOW));
        assert!(!session.matches_at("wrong", NOW));
        assert!(!session.matches_at(session.token(), session.expires_at()));
        assert_eq!(session.remaining_seconds_at(NOW + 0.75), 2_591_999);
        assert_eq!(session.remaining_seconds_at(session.expires_at()), 0);
        assert!(!format!("{session:?}").contains(session.token()));
    }

    #[test]
    fn derives_the_state_path_from_the_configuration_parent() {
        assert_eq!(
            web_session_path(Path::new("/tmp/emp/config.json")).expect("session path"),
            Path::new("/tmp/emp/state/web-session.json")
        );
    }
}
