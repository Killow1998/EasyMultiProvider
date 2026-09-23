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
    run_isolated_quota_process(auth, &trusted, timeout, allow_refresh, None, None)
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
) -> Result<String, QuotaError> {
    let auth = read_native_auth(auth_path)?;
    run_quota_reset(&auth, codex_binary, timeout, false, idempotency_key)
}

/// Consume one reset opportunity for a validated auth document.
pub fn run_quota_reset(
    auth: &Value,
    codex_binary: &str,
    timeout: Duration,
    allow_refresh: bool,
    idempotency_key: &str,
) -> Result<String, QuotaError> {
    let key = validated_reset_idempotency_key(idempotency_key)?;
    if account_auth_headers(auth).is_none() {
        return Err(QuotaError::new(
            "auth_json does not contain a ChatGPT access token",
            "quota_error",
        ));
    }
    let trusted = TrustedBinary::resolve(codex_binary)?;
    run_isolated_quota_process(auth, &trusted, timeout, allow_refresh, Some(&key), None).and_then(
        |result| {
            result
                .quota
                .get("outcome")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    QuotaError::new("Codex did not return a reset outcome", "quota_reset_failed")
                })
        },
    )
}

/// Imported-account reset variant that persists token rotation even when the
/// later consume RPC returns an authentication error.
pub fn run_quota_reset_persisting<F>(
    auth: &Value,
    codex_binary: &str,
    timeout: Duration,
    allow_refresh: bool,
    idempotency_key: &str,
    mut persist: F,
) -> Result<String, QuotaError>
where
    F: FnMut(&Value) -> Result<(), ()>,
{
    let key = validated_reset_idempotency_key(idempotency_key)?;
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

#[derive(Debug)]
struct TrustedBinary {
    path: PathBuf,
    identity: BinaryIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BinaryIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(not(unix))]
    Portable {
        length: u64,
        modified: Option<SystemTime>,
    },
}

impl TrustedBinary {
    fn resolve(name: &str) -> Result<Self, QuotaError> {
        let candidate = if Path::new(name).is_absolute() {
            PathBuf::from(name)
        } else {
            find_in_path(name).ok_or_else(binary_unavailable)?
        };
        let path = candidate.canonicalize().map_err(|_| binary_unavailable())?;
        validate_binary_path(&path)?;
        Ok(Self {
            identity: binary_identity(&path)?,
            path,
        })
    }

    fn verify(&self) -> Result<(), QuotaError> {
        let verified = Self::resolve(
            self.path
                .to_str()
                .ok_or_else(|| QuotaError::new("Codex executable was replaced", "quota_error"))?,
        )
        .map_err(|_| QuotaError::new("Codex executable was replaced", "quota_error"))?;
        if verified.path != self.path || verified.identity != self.identity {
            return Err(QuotaError::new(
                "Codex executable was replaced",
                "quota_error",
            ));
        }
        Ok(())
    }
}

fn binary_unavailable() -> QuotaError {
    QuotaError::new("Codex executable is unavailable", "quota_error")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(std::path::MAIN_SEPARATOR) {
        return None;
    }
    #[cfg(windows)]
    let names = if Path::new(name).extension().is_some() {
        vec![name.to_owned()]
    } else {
        env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("{name}{extension}"))
            .collect::<Vec<_>>()
    };
    #[cfg(not(windows))]
    let names = [name.to_owned()];
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        names.iter().find_map(|name| {
            let candidate = directory.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn validate_binary_path(path: &Path) -> Result<(), QuotaError> {
    let metadata = fs::metadata(path).map_err(|_| binary_unavailable())?;
    if !metadata.is_file() {
        return Err(QuotaError::new(
            "Codex executable is not trusted",
            "quota_error",
        ));
    }
    #[cfg(unix)]
    {
        let current_uid = unsafe { libc::getuid() };
        if metadata.mode() & 0o111 == 0 || (metadata.uid() != 0 && metadata.uid() != current_uid) {
            return Err(QuotaError::new(
                "Codex executable is not trusted",
                "quota_error",
            ));
        }
        for parent in path.ancestors() {
            let info = fs::metadata(parent)
                .map_err(|_| QuotaError::new("Codex executable is not trusted", "quota_error"))?;
            if info.mode() & 0o002 != 0
                || (info.mode() & 0o020 != 0 && info.uid() != 0 && info.uid() != current_uid)
            {
                return Err(QuotaError::new(
                    "Codex executable path is writable",
                    "quota_error",
                ));
            }
        }
    }
    Ok(())
}

fn binary_identity(path: &Path) -> Result<BinaryIdentity, QuotaError> {
    let metadata = fs::metadata(path).map_err(|_| binary_unavailable())?;
    #[cfg(unix)]
    {
        Ok(BinaryIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(BinaryIdentity::Portable {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

fn read_native_auth(path: &Path) -> Result<Value, QuotaError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| QuotaError::new("native auth.json is unavailable", "quota_error"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > MAX_AUTH_BYTES {
        return Err(QuotaError::new(
            "native auth.json is unavailable",
            "quota_error",
        ));
    }
    let raw = fs::read(path)
        .map_err(|_| QuotaError::new("native auth.json is unavailable", "quota_error"))?;
    let auth: Value = serde_json::from_slice(&raw)
        .map_err(|_| QuotaError::new("native auth.json is unavailable", "quota_error"))?;
    if account_auth_headers(&auth).is_none() {
        return Err(QuotaError::new(
            "native auth.json is unavailable",
            "quota_error",
        ));
    }
    Ok(auth)
}

fn run_isolated_quota_process(
    auth: &Value,
    binary: &TrustedBinary,
    timeout: Duration,
    allow_refresh: bool,
    reset_idempotency_key: Option<&str>,
    mut persist_rotation: Option<PersistRotation<'_>>,
) -> Result<QuotaProcessResult, QuotaError> {
    let directory = tempfile::Builder::new()
        .prefix("easy-mp-codex-account-")
        .tempdir()
        .map_err(|_| quota_check_failed())?;
    set_private_directory(directory.path())?;
    let auth_path = directory.path().join("auth.json");
    write_private(
        &auth_path,
        &serde_json::to_vec(auth).map_err(|_| quota_check_failed())?,
    )?;
    write_private(
        &directory.path().join("config.toml"),
        b"cli_auth_credentials_store = \"file\"\n",
    )?;

    binary.verify()?;
    let mut command = Command::new(&binary.path);
    command
        .arg("app-server")
        .arg("--stdio")
        .current_dir(directory.path())
        .env_clear()
        .env("CODEX_HOME", directory.path())
        .env("HOME", home_dir())
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in INHERITED_ENVIRONMENT {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().map_err(|_| quota_check_failed())?;
    let result = query_child(&mut child, timeout, allow_refresh, reset_idempotency_key);
    let cleanup = finish_child(&mut child);
    let refreshed_auth = fs::read(&auth_path)
        .ok()
        .filter(|raw| raw.len() <= MAX_AUTH_BYTES as usize)
        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
        .filter(|value| value != auth && account_auth_headers(value).is_some());
    if let (Some(persist), Some(refreshed)) = (&mut persist_rotation, &refreshed_auth) {
        persist(refreshed)?;
    }
    let output = match result {
        Ok(output) => {
            cleanup?;
            output
        }
        Err(error) => return Err(error),
    };
    Ok(QuotaProcessResult {
        quota: if reset_idempotency_key.is_some() {
            json!({"outcome": reset_outcome(&output, 3)?})
        } else {
            parse_app_server_output(&output)?
        },
        refreshed_auth,
    })
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn set_private_directory(path: &Path) -> Result<(), QuotaError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| quota_check_failed())?;
    Ok(())
}

fn write_private(path: &Path, value: &[u8]) -> Result<(), QuotaError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).map_err(|_| quota_check_failed())?;
    file.write_all(value).map_err(|_| quota_check_failed())?;
    file.flush().map_err(|_| quota_check_failed())
}

enum ProcessLine {
    Line(String),
    Encoding,
    Read,
    End,
}

fn stdout_reader(stdout: impl Read + Send + 'static) -> (Receiver<ProcessLine>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = sender.send(ProcessLine::End);
                    return;
                }
                Ok(_) => {
                    if sender.send(ProcessLine::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let kind = if error.kind() == std::io::ErrorKind::InvalidData {
                        ProcessLine::Encoding
                    } else {
                        ProcessLine::Read
                    };
                    let _ = sender.send(kind);
                    return;
                }
            }
        }
    });
    (receiver, worker)
}

fn query_child(
    child: &mut Child,
    timeout: Duration,
    allow_refresh: bool,
    reset_idempotency_key: Option<&str>,
) -> Result<String, QuotaError> {
    let stdout = child.stdout.take().ok_or_else(quota_check_failed)?;
    let stderr = child.stderr.take().ok_or_else(quota_check_failed)?;
    let (receiver, stdout_worker) = stdout_reader(stdout);
    let stderr_worker = thread::spawn(move || {
        let _ = std::io::copy(&mut BufReader::new(stderr), &mut std::io::sink());
    });
    let mut stdin = child.stdin.take().ok_or_else(quota_check_failed)?;
    let deadline = Instant::now() + timeout;
    let requests = [
        json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "easy-multi-provider",
                    "title": "EMP",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {},
            }
        }),
        json!({"method": "initialized"}),
        json!({"id": 2, "method": "account/read", "params": {"refreshToken": allow_refresh}}),
        reset_idempotency_key.map_or_else(
            || json!({"id": 3, "method": "account/rateLimits/read", "params": Value::Null}),
            |key| {
                json!({
                    "id": 3,
                    "method": "account/rateLimitResetCredit/consume",
                    "params": {"idempotencyKey": key},
                })
            },
        ),
    ];
    let mut output = String::new();
    let result = (|| {
        for request in requests {
            serde_json::to_writer(&mut stdin, &request).map_err(|_| quota_check_failed())?;
            stdin.write_all(b"\n").map_err(|_| quota_check_failed())?;
            stdin.flush().map_err(|_| quota_check_failed())?;
            let Some(id) = request.get("id").and_then(Value::as_i64) else {
                continue;
            };
            wait_for_response(&receiver, deadline, id, &request["method"], &mut output)?;
        }
        Ok(output)
    })();
    drop(stdin);
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = stdout_worker.join();
    let _ = stderr_worker.join();
    result
}

fn wait_for_response(
    receiver: &Receiver<ProcessLine>,
    deadline: Instant,
    request_id: i64,
    method: &Value,
    output: &mut String,
) -> Result<(), QuotaError> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(QuotaError::new(
                "Codex account quota check timed out",
                "quota_timeout",
            ));
        };
        let line = match receiver.recv_timeout(remaining) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                return Err(QuotaError::new(
                    "Codex account quota check timed out",
                    "quota_timeout",
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(QuotaError::new(
                    "Codex did not return account rate limits",
                    "quota_error",
                ));
            }
        };
        let line = match line {
            ProcessLine::Line(line) => line,
            ProcessLine::Encoding => {
                return Err(QuotaError::new(
                    "Codex app-server output is not valid UTF-8",
                    "quota_output_encoding_error",
                ));
            }
            ProcessLine::Read => {
                return Err(QuotaError::new(
                    "Could not read Codex app-server output",
                    "quota_output_read_error",
                ));
            }
            ProcessLine::End => {
                return Err(QuotaError::new(
                    "Codex did not return account rate limits",
                    "quota_error",
                ));
            }
        };
        output.push_str(&line);
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Some(message) = message.as_object() else {
            return Err(QuotaError::new(
                "Codex quota JSON-RPC output must be an object",
                "quota_output_protocol_error",
            ));
        };
        if message.get("id").and_then(Value::as_i64) != Some(request_id) {
            continue;
        }
        let method = method.as_str().unwrap_or_default();
        if let Some(error) = message.get("error") {
            return Err(quota_rpc_error(method, error));
        }
        if method == "account/read"
            && message
                .get("result")
                .and_then(Value::as_object)
                .is_some_and(|result| result.get("account") == Some(&Value::Null))
        {
            return Err(QuotaError::new(
                "Codex account needs sign-in; sign in again and re-import the account",
                "quota_auth_required",
            ));
        }
        return Ok(());
    }
}

fn finish_child(child: &mut Child) -> Result<(), QuotaError> {
    let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(quota_check_failed())
                };
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(quota_check_failed());
            }
        }
    }
}

fn quota_check_failed() -> QuotaError {
    QuotaError::new("Codex account quota check failed", "quota_error")
}

fn rpc_http_status(message: &str) -> Option<u16> {
    let public = message
        .split_once("; body=")
        .map_or(message, |(head, _)| head);
    for (index, _) in public.match_indices(" failed: ") {
        let tail = &public[index + " failed: ".len()..];
        let bytes = tail.as_bytes();
        if bytes.len() >= 3
            && bytes[..3].iter().all(u8::is_ascii_digit)
            && bytes.get(3).is_none_or(|byte| !byte.is_ascii_digit())
            && public[index..].contains("; content-type=")
        {
            return tail[..3].parse().ok();
        }
    }
    None
}

fn read_limits(
    payload: &Map<String, Value>,
    rate_limits: &mut Option<Map<String, Value>>,
    rate_limits_result: &mut Map<String, Value>,
    buckets: &mut Map<String, Value>,
) {
    if let Some(by_id) = payload
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        let current = by_id
            .iter()
            .filter(|(limit_id, value)| !limit_id.is_empty() && value.is_object())
            .map(|(limit_id, value)| (limit_id.clone(), value.clone()))
            .collect::<Map<_, _>>();
        if !current.is_empty() {
            *buckets = current;
            *rate_limits = buckets
                .get("codex")
                .or_else(|| buckets.values().next())
                .and_then(Value::as_object)
                .cloned();
            *rate_limits_result = payload.clone();
            return;
        }
    }
    if let Some(candidate) = payload.get("rateLimits").and_then(Value::as_object) {
        let limit_id = candidate
            .get("limitId")
            .or_else(|| candidate.get("limit_id"))
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned();
        buckets.insert(limit_id, Value::Object(candidate.clone()));
        *rate_limits = buckets
            .get("codex")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| Some(candidate.clone()));
        *rate_limits_result = payload.clone();
    }
}

fn mask_email(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let Some((local, domain)) = value.split_once('@') else {
        return String::new();
    };
    let first = local
        .chars()
        .next()
        .map_or("*".to_owned(), |value| value.to_string());
    format!("{first}***@{domain}")
}

fn safe_credit_snapshot(rate_limits: &Map<String, Value>, result: &Map<String, Value>) -> Value {
    let mut snapshot = Map::new();
    if let Some(credits) = rate_limits.get("credits").and_then(Value::as_object) {
        copy_renamed_scalar(credits, &mut snapshot, "hasCredits", "has_credits");
        copy_renamed_scalar(credits, &mut snapshot, "unlimited", "unlimited");
        copy_renamed_scalar(credits, &mut snapshot, "balance", "balance");
    }
    if let Some(limit) = rate_limits
        .get("individualLimit")
        .and_then(Value::as_object)
    {
        let mut safe = Map::new();
        for (source, target) in [
            ("limit", "limit"),
            ("used", "used"),
            ("remainingPercent", "remaining_percent"),
            ("resetsAt", "resets_at"),
        ] {
            if let Some(value) = limit.get(source).filter(|value| safe_scalar(value))
                && !value.is_null()
            {
                safe.insert(target.to_owned(), value.clone());
            }
        }
        if !safe.is_empty() {
            snapshot.insert("individual_limit".to_owned(), Value::Object(safe));
        }
    }
    if let Some(value) = rate_limits
        .get("spendControlReached")
        .filter(|value| value.is_boolean())
    {
        snapshot.insert("spend_control_reached".to_owned(), value.clone());
    }
    if let Some(value) = safe_reset_credits(result.get("rateLimitResetCredits")) {
        snapshot.insert("reset_credits".to_owned(), value);
    }
    if snapshot.is_empty() {
        Value::Null
    } else {
        Value::Object(snapshot)
    }
}

fn safe_reset_credits(value: Option<&Value>) -> Option<Value> {
    let value = value?.as_object()?;
    let mut snapshot = Map::new();
    if let Some(count) = value
        .get("availableCount")
        .filter(|count| count.as_i64().is_some() || count.as_u64().is_some())
    {
        snapshot.insert("available_count".to_owned(), count.clone());
    }
    if let Some(credits) = value.get("credits") {
        if credits.is_null() {
            snapshot.insert("credits".to_owned(), Value::Null);
        } else if let Some(credits) = credits.as_array() {
            let projected = credits
                .iter()
                .filter_map(Value::as_object)
                .map(|credit| {
                    let mut safe = Map::new();
                    for (source, target) in [
                        ("resetType", "reset_type"),
                        ("status", "status"),
                        ("grantedAt", "granted_at"),
                        ("expiresAt", "expires_at"),
                        ("title", "title"),
                        ("description", "description"),
                    ] {
                        if let Some(value) = credit.get(source).filter(|value| safe_scalar(value)) {
                            safe.insert(target.to_owned(), value.clone());
                        }
                    }
                    Value::Object(safe)
                })
                .collect();
            snapshot.insert("credits".to_owned(), Value::Array(projected));
        }
    }
    (!snapshot.is_empty()).then_some(Value::Object(snapshot))
}

fn copy_renamed_scalar(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    source_name: &str,
    target_name: &str,
) {
    if let Some(value) = source.get(source_name).filter(|value| safe_scalar(value)) {
        target.insert(target_name.to_owned(), value.clone());
    }
}

fn safe_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
assert pathlib.Path.cwd() == home
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
        assert request["params"] == {"idempotencyKey": "12345678-1234-4123-8123-123456789abc"}
        print(json.dumps({"id": request["id"], "result": {"outcome": "reset"}}), flush=True)
"#,
        )
        .expect("write fake Codex");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
            .expect("make fake Codex executable");
        let auth = json!({"tokens": {"access_token": "original-token", "account_id": "a"}});
        let trusted = TrustedBinary::resolve(script.to_str().expect("UTF-8 fake Codex path"))
            .expect("trusted fake Codex");
        let result =
            run_isolated_quota_process(&auth, &trusted, Duration::from_secs(5), false, None, None)
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
            )
            .expect("quota reset"),
            "reset"
        );
        let invalid =
            validated_reset_idempotency_key("retry-me").expect_err("non-UUID idempotency key");
        assert_eq!(invalid.code(), "quota_reset_invalid_request");
    }
}
