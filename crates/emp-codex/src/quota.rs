//! Safe projection of Codex app-server quota JSON-RPC output.

use crate::account_auth_headers;
use serde_json::{Map, Value, json};
use std::env;
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

mod process;
mod projection;
use process::{TrustedBinary, read_native_auth, rpc_http_status, run_isolated_quota_process};
#[cfg(test)]
use projection::safe_reset_credits;
use projection::{mask_email, read_limits, safe_credit_snapshot};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaError {
    message: String,
    code: &'static str,
    retry_imported_refresh: bool,
}

impl QuotaError {
    pub fn new(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            code,
            retry_imported_refresh: false,
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
pub fn read_native_login_quota(
    auth_path: &Path,
    codex_binary: &str,
    timeout: Duration,
) -> Result<Value, QuotaError> {
    let auth = read_native_auth(auth_path)?;
    run_quota_query(&auth, codex_binary, timeout, false).map(|result| result.quota)
}

/// Execute a quota read in an isolated Codex home.
///
/// A changed auth document is returned to the caller. The native-login helper
/// deliberately discards it; imported-account state decides whether and how to
/// persist it through EMP's encrypted vault.
pub fn run_quota_query(
    auth: &Value,
    codex_binary: &str,
    timeout: Duration,
    allow_refresh: bool,
) -> Result<QuotaProcessResult, QuotaError> {
    if account_auth_headers(auth).is_none() {
        return Err(QuotaError::new(
            "auth_json does not contain a ChatGPT access token",
            "quota_error",
        ));
    }
    let trusted = TrustedBinary::resolve(codex_binary)?;
    run_isolated_quota_process(auth, &trusted, timeout, allow_refresh, None, None, None)
}

/// Execute an imported-account quota read and save token rotation before a
/// later RPC error is returned to the caller.
pub fn run_quota_query_persisting<F>(
    auth: &Value,
    codex_binary: &str,
    timeout: Duration,
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

/// Classify one JSON-RPC error without exposing its upstream URL or body.
pub fn quota_rpc_error(method: &str, error: &Value) -> QuotaError {
    let message = error
        .as_object()
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auth_required = matches!(
        message,
        "codex account authentication required to read rate limits"
            | "chatgpt authentication required to read rate limits"
    ) || (method == "account/rateLimitResetCredit/consume"
        && message
            .to_ascii_lowercase()
            .contains("authentication required"));
    let status = rpc_http_status(message);
    if auth_required || status == Some(401) {
        return QuotaError::new(
            "Codex account needs sign-in; sign in again and re-import the account",
            "quota_auth_required",
        );
    }
    if status == Some(403) {
        return QuotaError::new(
            "Codex quota access was denied (403); check account access and network",
            "quota_access_denied",
        );
    }
    if status == Some(429) {
        return QuotaError::new(
            "Codex quota queries are rate limited (429); try again later",
            "quota_rate_limited",
        );
    }
    if method == "account/read"
        && [
            "workspace routing discovery timed out",
            "workspace routing discovery failed",
        ]
        .iter()
        .any(|expected| message.trim().eq_ignore_ascii_case(expected))
    {
        return QuotaError::new(
            "Codex could not reach ChatGPT workspace routing; check DNS, VPN/TUN, proxy, and network connectivity",
            "quota_transport_error",
        )
        .with_imported_refresh_retry();
    }
    match method {
        "account/rateLimits/read"
            if message
                .to_ascii_lowercase()
                .contains("error sending request") =>
        {
            QuotaError::new(
                "Codex could not connect to the quota service; check the proxy and network connection",
                "quota_transport_error",
            )
        }
        "account/rateLimits/read" => QuotaError::new(
            "Codex quota service query failed; check network connectivity and try again",
            "quota_fetch_failed",
        ),
        "account/read" => QuotaError::new("Codex account read failed", "quota_account_read_failed"),
        "account/rateLimitResetCredit/consume" => QuotaError::new(
            "Codex could not use the reset opportunity",
            "quota_reset_failed",
        ),
        _ => QuotaError::new(
            "Codex app-server initialization failed",
            "quota_initialize_failed",
        ),
    }
}

/// Parse one bounded app-server transcript into the browser-safe quota shape.
pub fn parse_app_server_output(output: &str) -> Result<Value, QuotaError> {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    parse_app_server_output_at(output, observed_at)
}

/// Deterministic variant used by the Python differential oracle.
pub fn parse_app_server_output_at(output: &str, observed_at: u64) -> Result<Value, QuotaError> {
    let mut account = Map::new();
    let mut rate_limits = None;
    let mut rate_limits_result = Map::new();
    let mut buckets = Map::new();

    for line in output.lines() {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Some(message) = message.as_object() else {
            return Err(QuotaError::new(
                "Codex quota JSON-RPC output must be an object",
                "quota_output_protocol_error",
            ));
        };
        if let Some(result) = message.get("result").and_then(Value::as_object) {
            if let Some(current) = result.get("account").and_then(Value::as_object) {
                account = current.clone();
            }
            read_limits(
                result,
                &mut rate_limits,
                &mut rate_limits_result,
                &mut buckets,
            );
        }
        if message.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated")
            && let Some(params) = message.get("params").and_then(Value::as_object)
        {
            read_limits(
                params,
                &mut rate_limits,
                &mut rate_limits_result,
                &mut buckets,
            );
        }
    }
    let Some(rate_limits) = rate_limits else {
        return Err(QuotaError::new(
            "Codex did not return account rate limits",
            "quota_error",
        ));
    };
    let plan_type = account
        .get("planType")
        .and_then(Value::as_str)
        .or_else(|| rate_limits.get("planType").and_then(Value::as_str))
        .map_or(Value::Null, |value| Value::String(value.to_owned()));
    let plan_type = if has_thirty_day_quota_window(&buckets) {
        Value::String("free".to_owned())
    } else {
        plan_type
    };
    Ok(json!({
        "account_label": mask_email(account.get("email")),
        "plan_type": plan_type,
        "rate_limits": rate_limits,
        "rate_limits_by_limit_id": buckets,
        "credits": safe_credit_snapshot(&rate_limits, &rate_limits_result),
        "updated_at": observed_at,
    }))
}

fn has_thirty_day_quota_window(buckets: &Map<String, Value>) -> bool {
    buckets.values().any(|bucket| {
        let Some(bucket) = bucket.as_object() else {
            return false;
        };
        ["primary", "secondary"].into_iter().any(|name| {
            let Some(window) = bucket.get(name).and_then(Value::as_object) else {
                return false;
            };
            let duration = window
                .get("windowDurationMins")
                .or_else(|| window.get("window_duration_mins"))
                .or_else(|| window.get("window_minutes"));
            duration.is_some_and(|duration| {
                duration.as_i64() == Some(43_200)
                    || duration.as_u64() == Some(43_200)
                    || duration.as_f64() == Some(43_200.0)
            })
        })
    })
}

/// Return the allowlisted reset outcome for one idempotent reset request.
pub fn reset_outcome(output: &str, request_id: i64) -> Result<&'static str, QuotaError> {
    for line in output.lines() {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Some(message) = message.as_object() else {
            return Err(QuotaError::new(
                "Codex reset JSON-RPC output must be an object",
                "quota_output_protocol_error",
            ));
        };
        if message.get("id").and_then(Value::as_i64) != Some(request_id) {
            continue;
        }
        let outcome = message
            .get("result")
            .and_then(Value::as_object)
            .and_then(|result| result.get("outcome"))
            .and_then(Value::as_str);
        return match outcome {
            Some("reset") => Ok("reset"),
            Some("nothingToReset") => Ok("nothingToReset"),
            Some("noCredit") => Ok("noCredit"),
            Some("alreadyRedeemed") => Ok("alreadyRedeemed"),
            _ => Err(QuotaError::new(
                "Codex did not return a reset outcome",
                "quota_reset_failed",
            )),
        };
    }
    Err(QuotaError::new(
        "Codex did not return a reset outcome",
        "quota_reset_failed",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn reset_credit_projection_preserves_only_usable_ids() {
        let source = json!({"credits": [
            {"id": " opaque ID ", "status": "available", "private": "drop"},
            {"id": "   ", "status": "available"},
            {"id": "x".repeat(257), "status": "available"},
            {"id": 42, "status": "available"},
        ]});
        let projected = safe_reset_credits(Some(&source)).expect("reset credits");
        assert_eq!(projected["credits"][0]["id"], " opaque ID ");
        assert!(projected["credits"][0].get("private").is_none());
        for index in 1..4 {
            assert!(projected["credits"][index].get("id").is_none());
        }
    }

    #[test]
    fn isolated_process_sequences_requests_and_returns_rotated_auth() {
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("target");
        fs::create_dir_all(&target).expect("create target directory");
        let root = tempfile::Builder::new()
            .prefix("emp-fake-codex-")
            .tempdir_in(target)
            .expect("temporary root");
        let script = root.path().join("fake-codex");
        fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys

home = pathlib.Path(os.environ["CODEX_HOME"])
assert pathlib.Path.cwd().samefile(home)
assert sys.argv[1:] == ["app-server", "--stdio"]
requests = []
for line in sys.stdin:
    request = json.loads(line)
    requests.append(request)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id": request["id"], "result": {}}), flush=True)
    elif method == "initialized":
        pass
    elif method == "account/read":
        assert request["params"] == {"refreshToken": False}
        auth = json.loads((home / "auth.json").read_text())
        auth["tokens"]["access_token"] = "rotated-token"
        (home / "auth.json").write_text(json.dumps(auth))
        print(json.dumps({"id": request["id"], "result": {"account": {"email": "xian@example.com", "planType": "pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        assert request["params"] is None
        print(json.dumps({"id": request["id"], "result": {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 7}}}}), flush=True)
    elif method == "account/rateLimitResetCredit/consume":
        auth = json.loads((home / "auth.json").read_text())
        expected = {"idempotencyKey": "12345678-1234-4123-8123-123456789abc"}
        if auth["tokens"]["account_id"] == "selected":
            expected["creditId"] = "opaque selected id"
        assert request["params"] == expected
        print(json.dumps({"id": request["id"], "result": {"outcome": "reset"}}), flush=True)
"#,
        )
        .expect("write fake Codex");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .expect("make fake Codex executable");
        let auth = json!({"tokens": {"access_token": "original-token", "account_id": "a"}});
        let trusted = TrustedBinary::resolve(script.to_str().expect("UTF-8 fake Codex path"))
            .expect("trusted fake Codex");
        let result = run_isolated_quota_process(
            &auth,
            &trusted,
            Duration::from_secs(5),
            false,
            None,
            None,
            None,
        )
        .expect("isolated quota query");
        assert_eq!(result.quota["account_label"], "x***@example.com");
        assert_eq!(result.quota["plan_type"], "pro");
        assert_eq!(result.quota["rate_limits"]["primary"]["usedPercent"], 7);
        assert_eq!(
            result.refreshed_auth.expect("rotated auth")["tokens"]["access_token"],
            "rotated-token"
        );
        assert_eq!(
            run_quota_reset(
                &auth,
                script.to_str().expect("UTF-8 fake Codex path"),
                Duration::from_secs(5),
                false,
                "12345678-1234-4123-8123-123456789ABC",
                None,
            )
            .expect("quota reset"),
            "reset"
        );
        let mut selected_auth = auth.clone();
        selected_auth["tokens"]["account_id"] = Value::String("selected".to_owned());
        assert_eq!(
            run_quota_reset(
                &selected_auth,
                script.to_str().expect("UTF-8 fake Codex path"),
                Duration::from_secs(5),
                false,
                "12345678-1234-4123-8123-123456789ABC",
                Some("opaque selected id"),
            )
            .expect("selected quota reset"),
            "reset"
        );
        let invalid =
            validated_reset_idempotency_key("retry-me").expect_err("non-UUID idempotency key");
        assert_eq!(invalid.code(), "quota_reset_invalid_request");
        let too_long = "x".repeat(257);
        for invalid in ["", " ", too_long.as_str()] {
            assert_eq!(
                validated_reset_credit_id(Some(invalid))
                    .expect_err("invalid reset credit id")
                    .code(),
                "quota_reset_invalid_request"
            );
        }
    }
}
