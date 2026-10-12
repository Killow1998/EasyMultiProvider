//! Safe projection of Codex app-server quota JSON-RPC output.

use crate::account_auth_headers;
use serde_json::{Map, Value, json};
use std::env;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

mod process;
mod projection;
use process::{TrustedBinary, read_native_auth, rpc_http_status, run_isolated_quota_process};
mod rpc;
pub use rpc::{
    parse_app_server_output, parse_app_server_output_at, quota_rpc_error, reset_outcome,
};

const MAX_AUTH_BYTES: u64 = 1024 * 1024;
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const INHERITED_ENVIRONMENT: &[&str] = &[
    "SYSTEMROOT",
    "LANG",
    "LC_ALL",
    "TERM",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CODEX_CA_CERTIFICATE",
    "NODE_EXTRA_CA_CERTS",
];
type PersistRotation<'a> = &'a mut dyn FnMut(&Value) -> Result<(), QuotaError>;

#[derive(Clone, Copy)]
pub struct QuotaControl<'a> {
    pub timeout: Duration,
    pub cancelled: Option<&'a AtomicBool>,
}
impl From<Duration> for QuotaControl<'_> {
    fn from(timeout: Duration) -> Self {
        Self {
            timeout,
            cancelled: None,
        }
    }
}
impl QuotaControl<'_> {
    fn check(self) -> Result<(), QuotaError> {
        if self
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            Err(QuotaError::new(
                "Quota check was cancelled",
                "quota_cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaError {
    message: String,
    code: &'static str,
    retry_imported_refresh: bool,
    rpc_method: Option<&'static str>,
    http_status: Option<u16>,
    transport_cause: Option<&'static str>,
}

impl QuotaError {
    pub fn new(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            code,
            retry_imported_refresh: false,
            rpc_method: None,
            http_status: None,
            transport_cause: None,
        }
    }

    fn with_imported_refresh_retry(mut self) -> Self {
        self.retry_imported_refresh = true;
        self
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }

    pub const fn should_retry_imported_refresh(&self) -> bool {
        self.retry_imported_refresh
    }

    /// Only classified fields, never the upstream error body, URL or credentials.
    pub fn diagnostics(&self) -> Value {
        json!({"code":self.code,"rpc_method":self.rpc_method,
            "http_status":self.http_status,"transport_cause":self.transport_cause})
    }
}

impl fmt::Display for QuotaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QuotaError {}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaProcessResult {
    pub quota: Value,
    pub refreshed_auth: Option<Value>,
}

/// Query the live native Codex login without modifying its auth file.
pub fn read_native_login_quota<'a>(
    auth_path: &Path,
    codex_binary: &str,
    control: impl Into<QuotaControl<'a>>,
) -> Result<Value, QuotaError> {
    let auth = read_native_auth(auth_path)?;
    run_quota_query(&auth, codex_binary, control, false).map(|result| result.quota)
}

/// Execute a quota read in an isolated Codex home.
///
/// A changed auth document is returned to the caller. The native-login helper
/// deliberately discards it; imported-account state decides whether and how to
/// persist it through EMP's encrypted vault.
pub fn run_quota_query<'a>(
    auth: &Value,
    codex_binary: &str,
    control: impl Into<QuotaControl<'a>>,
    allow_refresh: bool,
) -> Result<QuotaProcessResult, QuotaError> {
    if account_auth_headers(auth).is_none() {
        return Err(QuotaError::new(
            "auth_json does not contain a ChatGPT access token",
            "quota_error",
        ));
    }
    let control = control.into();
    control.check()?;
    let trusted = TrustedBinary::resolve(codex_binary)?;
    run_isolated_quota_process(auth, &trusted, control, allow_refresh, None, None, None)
}

/// Execute an imported-account quota read and save token rotation before a
/// later RPC error is returned to the caller.
pub fn run_quota_query_persisting<'a, F>(
    auth: &Value,
    codex_binary: &str,
    control: impl Into<QuotaControl<'a>>,
    allow_refresh: bool,
    mut persist: F,
) -> Result<Value, QuotaError>
where
    F: FnMut(&Value) -> Result<(), ()>,
{
    if account_auth_headers(auth).is_none() {
        return Err(QuotaError::new(
            "auth_json does not contain a ChatGPT access token",
            "quota_error",
        ));
    }
    let control = control.into();
    control.check()?;
    let trusted = TrustedBinary::resolve(codex_binary)?;
    let mut persist_rotation = |value: &Value| {
        persist(value).map_err(|()| {
            QuotaError::new(
                "Codex refreshed credentials could not be saved",
                "quota_credentials_save_failed",
            )
        })
    };
    run_isolated_quota_process(
        auth,
        &trusted,
        control,
        allow_refresh,
        None,
        None,
        Some(&mut persist_rotation),
    )
    .map(|result| result.quota)
}

/// Validate and consume one native reset opportunity without mutating the
/// native login document.
pub fn consume_native_quota_reset(
    auth_path: &Path,
    codex_binary: &str,
    timeout: Duration,
    idempotency_key: &str,
    credit_id: Option<&str>,
) -> Result<String, QuotaError> {
    let auth = read_native_auth(auth_path)?;
    run_quota_reset(
        &auth,
        codex_binary,
        timeout,
        false,
        idempotency_key,
        credit_id,
    )
}

/// Consume one reset opportunity for a validated auth document.
pub fn run_quota_reset(
    auth: &Value,
    codex_binary: &str,
    timeout: Duration,
    allow_refresh: bool,
    idempotency_key: &str,
    credit_id: Option<&str>,
) -> Result<String, QuotaError> {
    let key = validated_reset_idempotency_key(idempotency_key)?;
    let credit_id = validated_reset_credit_id(credit_id)?;
    if account_auth_headers(auth).is_none() {
        return Err(QuotaError::new(
            "auth_json does not contain a ChatGPT access token",
            "quota_error",
        ));
    }
    let trusted = TrustedBinary::resolve(codex_binary)?;
    run_isolated_quota_process(
        auth,
        &trusted,
        timeout,
        allow_refresh,
        Some(&key),
        credit_id,
        None,
    )
    .and_then(|result| {
        result
            .quota
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                QuotaError::new("Codex did not return a reset outcome", "quota_reset_failed")
            })
    })
}

/// Imported-account reset variant that persists token rotation even when the
/// later consume RPC returns an authentication error.
pub fn run_quota_reset_persisting<F>(
    auth: &Value,
    codex_binary: &str,
    timeout: Duration,
    allow_refresh: bool,
    idempotency_key: &str,
    credit_id: Option<&str>,
    mut persist: F,
) -> Result<String, QuotaError>
where
    F: FnMut(&Value) -> Result<(), ()>,
{
    let key = validated_reset_idempotency_key(idempotency_key)?;
    let credit_id = validated_reset_credit_id(credit_id)?;
    if account_auth_headers(auth).is_none() {
        return Err(QuotaError::new(
            "auth_json does not contain a ChatGPT access token",
            "quota_error",
        ));
    }
    let trusted = TrustedBinary::resolve(codex_binary)?;
    let mut persist_rotation = |value: &Value| {
        persist(value).map_err(|()| {
            QuotaError::new(
                "Codex refreshed credentials could not be saved",
                "quota_credentials_save_failed",
            )
        })
    };
    run_isolated_quota_process(
        auth,
        &trusted,
        timeout,
        allow_refresh,
        Some(&key),
        credit_id,
        Some(&mut persist_rotation),
    )
    .and_then(|result| {
        result
            .quota
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                QuotaError::new("Codex did not return a reset outcome", "quota_reset_failed")
            })
    })
}

pub fn validated_reset_idempotency_key(value: &str) -> Result<String, QuotaError> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !valid {
        return Err(QuotaError::new(
            "reset idempotency key is invalid",
            "quota_reset_invalid_request",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

pub fn validated_reset_credit_id(value: Option<&str>) -> Result<Option<&str>, QuotaError> {
    match value {
        Some(value) if value.trim().is_empty() || value.len() > 256 => Err(QuotaError::new(
            "reset credit id is invalid",
            "quota_reset_invalid_request",
        )),
        _ => Ok(value),
    }
}

#[cfg(all(test, unix))]
mod tests;
