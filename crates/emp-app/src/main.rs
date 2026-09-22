//! EMP native executable and local management HTTP surface.
//!
//! This bounded Rust slice serves the unchanged Web UI, the existing health
//! check, management APIs, and native/external Responses HTTP and WebSocket
//! traffic while the remaining Python behavior is migrated behind the same UI.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use emp_codex::quota::{
    QuotaError, consume_native_quota_reset, read_native_login_quota, run_quota_query_persisting,
    run_quota_reset_persisting,
};
use emp_codex::quota_history::{QuotaHistoryError, QuotaHistoryStore};
use emp_codex::{account_auth_headers, subscription_route_model};
use emp_core::{ResolvedRoute, RouteResolutionError, resolve_route};
use emp_history::HistoryError;
use emp_router::native_http::NativeStream;
use emp_router::native_metadata::native_response_headers;
use emp_router::{
    ExternalRouter, ExternalStream, ProjectionIds, RouterError, RouterErrorKind,
    StreamResponseEvent, project_external_payload, protocol_candidates,
    response_json_stream_events,
};
use emp_state::{
    ConfigError, ExportGroups, FileTransaction, FilesystemError, VaultStore,
    WEB_SESSION_TOKEN_BYTES, WebSession, WebSessionError, config_path,
    export_migration_bundle_with_summary, import_migration_bundle, load_configuration,
    load_or_create_web_session, normalize_account, normalize_configuration, provider_api_key,
    public_configuration_with_file_status, remember_resolved_protocol, same_account_auth,
    save_configuration, save_configuration_in_transaction, validate_auth_json, web_session_path,
};
use emp_transport::{
    ClientWebSocket, ContentDecodeError, FailureClass, FailurePhase, HttpClient, HttpClientPolicy,
    HttpMethod, ProxyEnvironment, ProxyPolicy, RequestCapacityError, RequestLimits,
    RequestLimitsConfig, RequestLimitsError, TimeoutPolicy, TransportKind, UpstreamFailure,
    WebSocketConnection, decode_content, external_http_retry_allowed, normalize_error_class,
    protocol_fallback_allowed, public_failure_message, websocket_accept,
};
use serde_json::Value;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

mod catalog_api;
mod native_api;

pub const VERSION: &str = "0.11.6";

/// Embedded directly from the existing Python package so Web UI bytes cannot
/// drift during the rewrite.
pub const WEB_INDEX_BYTES: &[u8] = include_bytes!("../../../easy_multi_provider/web/index.html");

/// Exact compact JSON body observed in the Python server tests.
pub const HEALTH_JSON_BYTES: &[u8] = b"{\"status\":\"ok\"}";

const LOGIN_HTML: &str = r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>登录 EMP</title><body style="font-family:system-ui;max-width:36rem;margin:12vh auto;padding:24px;line-height:1.7"><h1>请从 EMP 打开管理页</h1><p>此浏览器尚未登录，或登录已过期。</p><p>请打开 EMP 启动时自动弹出的网页；也可以使用终端中 Open in browser 后的完整链接。</p><p>登录有效期为 30 天，期间重启 EMP 无需重新登录。</p></body></html>"#;

const LOGIN_HTML_BYTES: &[u8] = LOGIN_HTML.as_bytes();

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_PRE_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_PRE_OUTPUT_BUFFER_EVENTS: usize = 256;
const QUOTA_EVENT_SLOT_LIMIT: usize = 4;
const QUOTA_EVENT_KEEP_ALIVE: Duration = Duration::from_secs(15);
const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS: usize = 256 * 1024;
const COMPACTION_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another language model that will resume the task.\n\nInclude current progress, key decisions, constraints, user preferences, remaining steps, and critical data or references. Be concise, structured, and focused on seamless continuation.";

#[derive(Debug)]
pub enum AppError {
    HostNotLoopback,
    Io(std::io::Error),
    ServerStopped,
    RandomUnavailable,
    WebSession(WebSessionError),
    Config(ConfigError),
    Filesystem(FilesystemError),
    Transport(emp_transport::HttpTransportError),
    RequestLimits(RequestLimitsError),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostNotLoopback => {
                formatter.write_str("host must be 127.0.0.1 for local-only management")
            }
            Self::Io(error) => write!(formatter, "{error}"),
            Self::ServerStopped => formatter.write_str("server task stopped before shutdown"),
            Self::RandomUnavailable => formatter.write_str("secure randomness is unavailable"),
            Self::WebSession(error) => write!(formatter, "{error}"),
            Self::Config(error) => write!(formatter, "{error}"),
            Self::Filesystem(error) => write!(formatter, "{error}"),
            Self::Transport(error) => write!(formatter, "{error}"),
            Self::RequestLimits(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<WebSessionError> for AppError {
    fn from(value: WebSessionError) -> Self {
        Self::WebSession(value)
    }
}

impl From<ConfigError> for AppError {
    fn from(value: ConfigError) -> Self {
        Self::Config(value)
    }
}

impl From<FilesystemError> for AppError {
    fn from(value: FilesystemError) -> Self {
        Self::Filesystem(value)
    }
}

impl From<emp_transport::HttpTransportError> for AppError {
    fn from(value: emp_transport::HttpTransportError) -> Self {
        Self::Transport(value)
    }
}

impl From<RequestLimitsError> for AppError {
    fn from(value: RequestLimitsError) -> Self {
        Self::RequestLimits(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    Version,
    Serve {
        config: Option<PathBuf>,
        host: IpAddr,
        port: u16,
    },
}

pub fn parse_cli<I>(arguments: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err(
            "usage: EMP [--version] | serve [--config PATH] --host HOST --port PORT".to_string(),
        );
    };
    if command == "--version" {
        return Ok(Cli::Version);
    }
    if command != "serve" {
        return Err(format!("unknown command: {command}"));
    }

    let mut config = None;
    let mut host = None;
    let mut port = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--config" => {
                if config.is_some() {
                    return Err("--config was provided more than once".to_string());
                }
                config = Some(PathBuf::from(
                    arguments.next().ok_or("--config requires a value")?,
                ));
            }
            "--host" => {
                if host.is_some() {
                    return Err("--host was provided more than once".to_string());
                }
                host = Some(arguments.next().ok_or("--host requires a value")?);
            }
            "--port" => {
                if port.is_some() {
                    return Err("--port was provided more than once".to_string());
                }
                let raw = arguments.next().ok_or("--port requires a value")?;
                port = Some(
                    raw.parse::<u16>()
                        .map_err(|_| format!("invalid port: {raw}"))?,
                );
            }
            unknown => return Err(format!("unknown serve option: {unknown}")),
        }
    }

    let host = host.ok_or("serve requires --host")?;
    let port = port.ok_or("serve requires --port")?;
    let host = host
        .parse::<IpAddr>()
        .map_err(|_| format!("invalid host: {host}"))?;
    if !is_loopback(host) {
        return Err(AppError::HostNotLoopback.to_string());
    }
    Ok(Cli::Serve { config, host, port })
}

pub fn is_loopback(host: IpAddr) -> bool {
    host == IpAddr::V4(Ipv4Addr::LOCALHOST)
        || matches!(host, IpAddr::V6(address) if address.is_loopback())
}

struct BootstrapToken {
    token: String,
    used: AtomicBool,
}

impl BootstrapToken {
    fn matches(&self, supplied: &str) -> bool {
        constant_time_eq(supplied.as_bytes(), self.token.as_bytes())
    }

    fn consume(&self) -> bool {
        self.used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

struct SessionStore {
    session: Mutex<WebSession>,
    path: PathBuf,
}

impl SessionStore {
    fn contains(&self, supplied: Option<&str>, now: f64) -> bool {
        let Some(supplied) = supplied else {
            return false;
        };
        let Ok(session) = self.session.lock() else {
            return false;
        };
        session.matches_at(supplied, now)
    }

    fn refresh_header(&self, now: f64) -> Option<String> {
        let session = self.session.lock().ok()?;
        if !session.is_active_at(now) {
            return None;
        }
        Some(session_cookie(
            session.token(),
            session.remaining_seconds_at(now),
        ))
    }

    fn refresh_or_rotate_header(&self, now: f64) -> Option<String> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestMethod {
    Get,
    Head,
    Post,
    Delete,
}

#[derive(Clone, Copy)]
struct Request<'a> {
    method: RequestMethod,
    target: &'a str,
    headers: &'a str,
}

impl<'a> Request<'a> {
    fn header(&self, name: &str) -> Option<&'a str> {
        self.headers.lines().skip(1).find_map(|line| {
            let (header_name, value) = line.split_once(':')?;
            header_name
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim())
        })
    }

    fn raw_path(&self) -> &'a str {
        self.target
            .split_once('?')
            .map_or(self.target, |(path, _)| path)
    }

    fn session_cookie(&self) -> Option<String> {
        parse_session_cookie(self.header("Cookie")?)
    }
}

fn parse_session_cookie(header: &str) -> Option<String> {
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

fn system_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn session_cookie(token: &str, max_age: u64) -> String {
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

fn response(
    status_line: &str,
    content_type: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut response = Vec::with_capacity(body.len() + 256);
    let _ = write!(
        response,
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if content_type == "application/json" {
        response.extend_from_slice(b"Cache-Control: no-store\r\n");
    }
    for (name, value) in headers {
        let _ = write!(response, "{name}: {value}\r\n");
    }
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(body);
    response
}

fn health_response() -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        HEALTH_JSON_BYTES,
        &[],
    )
}

fn ui_response(cookie: &str) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "text/html; charset=utf-8",
        WEB_INDEX_BYTES,
        &[("Cache-Control", "no-store"), ("Set-Cookie", cookie)],
    )
}

fn login_response() -> Vec<u8> {
    response(
        "HTTP/1.1 401 Unauthorized",
        "text/html; charset=utf-8",
        LOGIN_HTML_BYTES,
        &[("Cache-Control", "no-store")],
    )
}

fn redirect_response(cookie: &str) -> Vec<u8> {
    response(
        "HTTP/1.1 303 See Other",
        "text/plain; charset=utf-8",
        b"",
        &[("Location", "/"), ("Set-Cookie", cookie)],
    )
}

fn not_found_response() -> Vec<u8> {
    response(
        "HTTP/1.1 404 Not Found",
        "application/json",
        b"{\"error\":{\"message\":\"not found\"}}",
        &[],
    )
}

fn unauthorized_response() -> Vec<u8> {
    response(
        "HTTP/1.1 401 Unauthorized",
        "application/json",
        b"{\"error\":{\"message\":\"management session is required\"}}",
        &[],
    )
}

fn cross_origin_response(message: &str) -> Vec<u8> {
    response(
        "HTTP/1.1 403 Forbidden",
        "application/json",
        format!("{{\"error\":{{\"message\":\"{message}\"}}}}").as_bytes(),
        &[],
    )
}

fn bad_request_response() -> Vec<u8> {
    response(
        "HTTP/1.1 400 Bad Request",
        "application/json",
        b"{\"error\":{\"message\":\"invalid HTTP request\"}}",
        &[],
    )
}

fn json_error_response(
    status: u16,
    status_text: &str,
    message: &str,
    code: Option<&str>,
    headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut error = serde_json::Map::new();
    if let Some(code) = code {
        error.insert("code".to_owned(), Value::String(code.to_owned()));
    }
    error.insert("message".to_owned(), Value::String(message.to_owned()));
    let body = serde_json::to_vec(&serde_json::json!({"error": error}))
        .expect("error response is JSON serializable");
    response(
        &format!("HTTP/1.1 {status} {status_text}"),
        "application/json",
        &body,
        headers,
    )
}

struct RequestHead {
    head: String,
    body_prefix: Vec<u8>,
}

fn read_request_head(stream: &mut TcpStream) -> Option<RequestHead> {
    let mut buffer = [0_u8; 1024];
    let mut request = Vec::new();
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(separator) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            if separator > MAX_HEADER_BYTES {
                return None;
            }
            let body_prefix = request.split_off(separator + 4);
            request.truncate(separator + 4);
            return Some(RequestHead {
                head: String::from_utf8(request).ok()?,
                body_prefix,
            });
        }
        if request.len() > MAX_HEADER_BYTES + 3 {
            return None;
        }
    }
    None
}

fn request_line(request: &str) -> Option<(RequestMethod, &str)> {
    let request_line = request.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = match parts.next()? {
        "GET" => RequestMethod::Get,
        "HEAD" => RequestMethod::Head,
        "POST" => RequestMethod::Post,
        "DELETE" => RequestMethod::Delete,
        _ => return None,
    };
    let target = parts.next()?;
    (parts.next()? == "HTTP/1.1" && parts.next().is_none()).then_some((method, target))
}

fn parse_request(request: &str) -> Option<Request<'_>> {
    let headers_end = request.find("\r\n\r\n")?;
    let (method, target) = request_line(request)?;
    Some(Request {
        method,
        target,
        headers: &request[..headers_end],
    })
}

fn percent_decode(raw: &str, plus_to_space: bool) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push((high * 16 + low) as u8);
                    index += 3;
                } else {
                    decoded.push(b'%');
                    index += 1;
                }
            }
            b'+' if plus_to_space => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn query_values(target: &str, name: &str) -> Vec<String> {
    let Some((_, query)) = target.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| percent_decode(key, true) == name)
        .map(|(_, value)| percent_decode(value, true))
        .collect()
}

fn bootstrap_value(target: &str) -> Option<String> {
    let values = query_values(target, "bootstrap");
    (values.len() == 1 && values[0].is_ascii()).then(|| values[0].clone())
}

fn same_origin(request: Request<'_>, port: u16) -> bool {
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

const MAX_NATIVE_AUTH_BYTES: usize = 1024 * 1024;

fn codex_auth_path() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| PathBuf::from(home).join(".codex"))
        })
        .unwrap_or_else(|| PathBuf::from(".codex"))
        .join("auth.json")
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

fn valid_caller_authorization(value: Option<&str>, auth_path: &Path) -> bool {
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

fn proxy_allowed(request: Request<'_>, state: &ServerState, now: f64) -> bool {
    if !same_origin(request, state.port) {
        return false;
    }
    let supplied_cookie = request.session_cookie();
    state.sessions.contains(supplied_cookie.as_deref(), now)
        || valid_caller_authorization(
            request.header("Authorization"),
            &state.backend.native_auth_path,
        )
}

fn random_hex(bytes: usize) -> Result<String, AppError> {
    let mut raw = vec![0_u8; bytes];
    getrandom::getrandom(&mut raw).map_err(|_| AppError::RandomUnavailable)?;
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in raw {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

fn projection_ids() -> Result<ProjectionIds, AppError> {
    Ok(ProjectionIds::new(
        format!("resp_{}", random_hex(16)?),
        format!("msg_{}", random_hex(16)?),
        format!("rs_{}", random_hex(16)?),
        format!("rs_{}", random_hex(16)?),
    ))
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

fn capacity_response(error: &RequestCapacityError) -> Vec<u8> {
    let capacity = error.http_status() == 503;
    let code = if capacity {
        "request_capacity_unavailable"
    } else {
        "request_too_large"
    };
    let mut detail = serde_json::json!({
        "code": code,
        "message": error.to_string(),
        "limit_bytes": error.limit,
    });
    if capacity {
        detail["memory"] = serde_json::json!({
            "used_percent": error.memory_used_percent,
            "used_bytes": error.memory_used_bytes,
            "total_bytes": error.memory_total_bytes,
            "available_bytes": error.available_bytes,
            "required_bytes": error.required_memory_bytes,
        });
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail}))
        .expect("capacity response is JSON serializable");
    let retry = capacity.then_some(("Retry-After", "2"));
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.http_status(),
            status_text(error.http_status())
        ),
        "application/json",
        &body,
        &retry.into_iter().collect::<Vec<_>>(),
    )
}

enum BodyError {
    Invalid(String),
    Capacity(RequestCapacityError),
    Decode(ContentDecodeError),
}

fn body_size_error(limit: usize, decoded: bool) -> BodyError {
    BodyError::Capacity(RequestCapacityError {
        limit,
        decoded,
        reason: emp_transport::RequestCapacityReason::HardLimit,
        available_bytes: 0,
        required_memory_bytes: 0,
        memory_total_bytes: 0,
        memory_used_bytes: 0,
        memory_used_percent: None,
    })
}

fn read_json_body(
    stream: &mut TcpStream,
    request: Request<'_>,
    mut body: Vec<u8>,
    state: &ServerState,
) -> Result<Value, BodyError> {
    let content_type = request
        .header("Content-Type")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if !content_type.eq_ignore_ascii_case("application/json") {
        return Err(BodyError::Invalid(
            "Content-Type must be application/json".to_owned(),
        ));
    }
    let raw_length = request.header("Content-Length").unwrap_or("0");
    if raw_length.starts_with('-') {
        if raw_length.parse::<i128>().is_ok_and(|value| value < 0) {
            return Err(BodyError::Invalid(
                "Content-Length cannot be negative".to_owned(),
            ));
        }
        return Err(BodyError::Invalid("invalid Content-Length".to_owned()));
    }
    let length = raw_length
        .parse::<u128>()
        .map_err(|_| BodyError::Invalid("invalid Content-Length".to_owned()))?;
    // Python attaches a dynamic request budget only to model-generation paths.
    // Management requests retain their fixed wire and decoded-body ceiling.
    let management = request.raw_path().starts_with("/api/");
    let limit = if management {
        5 * 1024 * 1024
    } else {
        RequestLimitsConfig::default().maximum
    };
    let length = usize::try_from(length).map_err(|_| body_size_error(limit, false))?;
    let mut budget =
        (!management).then(|| state.backend.request_limits.request(TransportKind::Http));
    if let Some(budget) = &mut budget {
        budget.ensure(length).map_err(BodyError::Capacity)?;
    } else if length > limit {
        return Err(body_size_error(limit, false));
    }
    if body.len() > length {
        body.truncate(length);
    }
    while body.len() < length {
        let remaining = length - body.len();
        let mut chunk = [0_u8; 64 * 1024];
        let read_length = remaining.min(chunk.len());
        let count = stream
            .read(&mut chunk[..read_length])
            .map_err(|_| BodyError::Invalid("request body is incomplete".to_owned()))?;
        if count == 0 {
            return Err(BodyError::Invalid("request body is incomplete".to_owned()));
        }
        body.extend_from_slice(&chunk[..count]);
    }
    let body = decode_content(
        body,
        request.header("Content-Encoding").unwrap_or_default(),
        limit,
        budget.as_mut(),
    )
    .map_err(|error| match error {
        ContentDecodeError::DecodedTooLarge { limit } => body_size_error(limit, true),
        error => BodyError::Decode(error),
    })?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|error| BodyError::Invalid(format!("request body must be valid JSON: {error}")))?;
    if !value.is_object() {
        return Err(BodyError::Invalid(
            "request body must be a JSON object".to_owned(),
        ));
    }
    Ok(value)
}

fn body_error_response(error: BodyError) -> Vec<u8> {
    match error {
        BodyError::Invalid(message) => {
            json_error_response(400, status_text(400), &message, None, &[])
        }
        BodyError::Capacity(error) => capacity_response(&error),
        BodyError::Decode(ContentDecodeError::Capacity(error)) => capacity_response(&error),
        BodyError::Decode(error) => json_error_response(
            error.http_status(),
            status_text(error.http_status()),
            &error.to_string(),
            (error.http_status() == 413).then_some("request_too_large"),
            &[],
        ),
    }
}

fn hydrate_provider_keys(config: &mut Value, vault: &VaultStore) {
    let Some(providers) = config.get_mut("providers").and_then(Value::as_array_mut) else {
        return;
    };
    for provider in providers {
        let key = provider_api_key(provider, vault);
        if let Some(provider) = provider.as_object_mut() {
            provider.insert("api_key".to_owned(), Value::String(key));
        }
    }
}

fn account_catalog_headers(
    account: &serde_json::Map<String, Value>,
    vault: &VaultStore,
) -> Option<BTreeMap<String, String>> {
    let path = account
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())?;
    let auth = vault.read_encrypted_json(Path::new(path)).ok()?;
    account_auth_headers(&auth)
}

fn route_resolution_response(error: RouteResolutionError) -> Vec<u8> {
    let error_class = if error.status() >= 500 {
        "upstream_5xx"
    } else {
        "router_error"
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": error_class,
            "type": error_class,
            "message": error.to_string(),
        }
    }))
    .expect("route resolution response is JSON serializable");
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &[],
    )
}

fn request_router_error_response(status: u16, message: &str) -> Vec<u8> {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": "router_error",
            "type": "router_error",
            "message": message,
        }
    }))
    .expect("request router response is JSON serializable");
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &body,
        &[],
    )
}

fn router_error_response(error: RouterError) -> Vec<u8> {
    let failure_reason = error.failure_reason().map(str::to_owned);
    let error_class = error.error_class().as_str();
    let code = if error_class == "rate_limit" {
        "rate_limit_exceeded".to_owned()
    } else {
        failure_reason
            .clone()
            .unwrap_or_else(|| error_class.to_owned())
    };
    let mut detail = serde_json::json!({
        "code": code,
        "type": error_class,
        "message": error.to_string(),
    });
    if let Some(reason) = failure_reason {
        detail["failure_reason"] = Value::String(reason);
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail}))
        .expect("router response is JSON serializable");
    let retry = error.retry_after_seconds().map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &headers,
    )
}

fn stream_error_code(error_class: FailureClass) -> &'static str {
    match error_class {
        FailureClass::ContextLengthExceeded => "context_length_exceeded",
        FailureClass::PaymentRequired => "payment_required",
        FailureClass::RateLimit => "rate_limit_exceeded",
        _ => "upstream_error",
    }
}

fn safe_failure_reason(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn stream_failure_value(error: &RouterError, response_id: &str) -> Value {
    let error_class = error.error_class();
    let mut detail = serde_json::json!({
        "code": stream_error_code(error_class),
        "message": format!(
            "HTTP {}: {}",
            error.status(),
            public_failure_message(error_class, error.failure_reason(), error.status())
        ),
        "status": error.status(),
        "error_class": error_class.as_str(),
    });
    if let Some(reason) = error.failure_reason() {
        let reason = safe_failure_reason(reason);
        if !reason.is_empty() {
            detail["failure_reason"] = Value::String(reason);
        }
    }
    if error.kind() == RouterErrorKind::Transport {
        detail["transport_failure"] = Value::Bool(true);
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
        if error_class == FailureClass::RateLimit {
            detail["message"] = Value::String(format!(
                "{} Please try again in {delay}s.",
                detail["message"].as_str().unwrap_or_default()
            ));
        }
    }
    serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "failed",
            "error": detail,
        }
    })
}

fn sse_frame(event: &str, body: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let compact = serde_json::to_vec(body)?;
    let mut data = Vec::with_capacity(compact.len() + compact.len() / 8);
    let mut in_string = false;
    let mut escaped = false;
    for byte in compact {
        data.push(byte);
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if matches!(byte, b',' | b':') {
            data.push(b' ');
        }
    }
    let mut frame = Vec::with_capacity(event.len() + data.len() + 16);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    frame.extend_from_slice(&data);
    frame.extend_from_slice(b"\n\n");
    Ok(frame)
}

fn stream_event_activity(event: &Value) -> (bool, bool) {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let item = event.get("item").and_then(Value::as_object);
    let item_type = item
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_activity = matches!(
        item_type,
        "function_call" | "custom_tool_call" | "tool_call" | "tool_search_call"
    ) || event_type.contains("function_call")
        || event_type.contains("tool_call");
    let mut output_emitted = tool_activity;
    if event_type.ends_with(".delta") || event_type.ends_with(".done") {
        output_emitted |= [
            "output_text",
            "output_image",
            "image_generation",
            "reasoning",
        ]
        .iter()
        .any(|marker| event_type.contains(marker));
    } else {
        output_emitted |=
            event_type.contains("output_image") || event_type.contains("image_generation");
    }
    output_emitted |= event
        .get("part")
        .and_then(Value::as_object)
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|part_type| {
            matches!(
                part_type,
                "output_text" | "output_image" | "reasoning_text" | "summary_text"
            )
        });
    if let Some(content) = item
        .and_then(|item| item.get("content"))
        .and_then(Value::as_array)
    {
        output_emitted |= content.iter().any(|part| {
            part.get("type")
                .and_then(Value::as_str)
                .is_some_and(|part_type| {
                    matches!(
                        part_type,
                        "output_text" | "output_image" | "image" | "image_url" | "reasoning_text"
                    )
                })
        });
    }
    (output_emitted, tool_activity)
}

fn terminal_stream_event(event: &Value) -> bool {
    event
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|event_type| {
            matches!(
                event_type,
                "response.completed" | "response.incomplete" | "response.failed" | "error"
            )
        })
}

fn pre_output_failure_response(event: &Value) -> Option<Vec<u8>> {
    if event.get("type").and_then(Value::as_str) != Some("response.failed") {
        return None;
    }
    let error = event.get("response")?.get("error")?.as_object()?;
    let status = error
        .get("status")?
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())?;
    if !(400..=599).contains(&status) {
        return None;
    }
    let error_class_name = error
        .get("error_class")
        .and_then(Value::as_str)
        .unwrap_or("upstream_error");
    let error_class = normalize_error_class(Some(error_class_name), FailureClass::StreamError);
    let failure_reason = error.get("failure_reason").and_then(Value::as_str);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_else(|| stream_error_code(error_class));
    let mut detail = serde_json::json!({
        "type": error_class_name,
        "code": code,
        "message": public_failure_message(error_class, failure_reason, status),
        "param": Value::Null,
    });
    if let Some(reason) = failure_reason {
        detail["failure_reason"] = Value::String(reason.to_owned());
    }
    let retry_after = error.get("retry_after_seconds").and_then(Value::as_u64);
    if let Some(delay) = retry_after {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail})).ok()?;
    let retry = retry_after.map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    Some(response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &body,
        &headers,
    ))
}

fn pre_output_router_error_response(error: &RouterError) -> Vec<u8> {
    let error_class = error.error_class();
    let mut detail = serde_json::json!({
        "type": error_class.as_str(),
        "code": stream_error_code(error_class),
        "message": public_failure_message(error_class, error.failure_reason(), error.status()),
        "param": Value::Null,
    });
    if let Some(reason) = error.failure_reason()
        && error_class != FailureClass::StreamIncomplete
    {
        detail["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail}))
        .expect("stream error response is JSON serializable");
    let retry = error.retry_after_seconds().map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &headers,
    )
}

fn external_retry_delay(
    error: &RouterError,
    attempt: usize,
    route: &ResolvedRoute,
) -> Option<Duration> {
    let failure = UpstreamFailure {
        error_class: error.error_class(),
        status: error.status(),
        phase: FailurePhase::TerminalValidation,
        terminal_event: false,
        failure_reason: error.failure_reason().map(str::to_owned),
        retry_after_seconds: error.retry_after_seconds(),
    };
    let free_route = route
        .upstream_model
        .trim()
        .to_ascii_lowercase()
        .ends_with(":free");
    external_http_retry_allowed(&failure, attempt, false, false, free_route)
        .then(|| Duration::from_secs(failure.retry_after_seconds.unwrap_or(1)))
}

struct DisconnectMonitor {
    disconnected: tokio::sync::oneshot::Receiver<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DisconnectMonitor {
    fn start(stream: &TcpStream) -> std::io::Result<Self> {
        let probe = stream.try_clone()?;
        probe.set_read_timeout(Some(Duration::from_millis(50)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (sender, disconnected) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            let mut byte = [0_u8; 1];
            while !worker_stop.load(Ordering::Acquire) {
                match probe.peek(&mut byte) {
                    Ok(0) => {
                        let _ = sender.send(());
                        return;
                    }
                    Ok(_) => thread::sleep(Duration::from_millis(10)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => {
                        let _ = sender.send(());
                        return;
                    }
                }
            }
        });
        Ok(Self {
            disconnected,
            stop,
            worker: Some(worker),
        })
    }

    async fn next_event(&mut self, stream: &mut ExternalStream) -> StreamPoll {
        tokio::select! {
            result = stream.next_event() => StreamPoll::Event(result),
            _ = &mut self.disconnected => StreamPoll::Disconnected,
        }
    }

    async fn next_native_event(&mut self, stream: &mut NativeStream) -> NativeStreamPoll {
        tokio::select! {
            result = stream.next_event() => NativeStreamPoll::Event(result),
            _ = &mut self.disconnected => NativeStreamPoll::Disconnected,
        }
    }
}

impl Drop for DisconnectMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum StreamPoll {
    Event(Result<Option<StreamResponseEvent>, RouterError>),
    Disconnected,
}

enum NativeStreamPoll {
    Event(Result<Option<emp_router::native_http::NativeStreamEvent>, RouterError>),
    Disconnected,
}

fn write_stream_head(stream: &mut TcpStream) -> std::io::Result<()> {
    write_stream_head_with_headers(stream, &BTreeMap::new())
}

fn write_stream_head_with_headers(
    stream: &mut TcpStream,
    headers: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n",
    )?;
    for (name, value) in headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "content-length" | "connection" | "cache-control"
        ) {
            continue;
        }
        stream.write_all(name.as_bytes())?;
        stream.write_all(b": ")?;
        stream.write_all(value.as_bytes())?;
        stream.write_all(b"\r\n")?;
    }
    stream.write_all(b"Connection: close\r\n\r\n")
}

fn write_stream_frames(stream: &mut TcpStream, frames: &[Vec<u8>]) -> std::io::Result<()> {
    for frame in frames {
        stream.write_all(frame)?;
    }
    stream.flush()
}

fn persist_protocol_observation(state: &ServerState, route: &ResolvedRoute) {
    let Ok(mut config) = state.backend.config.lock() else {
        return;
    };
    let Ok(Some(updated)) = remember_resolved_protocol(
        &config,
        &route.provider_id,
        &route.requested_model,
        route.protocol.as_config_str(),
    ) else {
        return;
    };
    if save_configuration(
        &updated,
        Some(&state.backend.config_path),
        &state.backend.vault,
    )
    .is_err()
    {
        return;
    }
    if let Ok(reloaded) = load_configuration(Some(&state.backend.config_path)) {
        *config = reloaded;
    }
}

fn serve_external_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let router = ExternalRouter::new(&state.backend.client);
    let candidates = protocol_candidates(route);
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(route_resolution_response)?;
        for attempt in 0..2 {
            match state
                .backend
                .runtime
                .block_on(router.open_stream(&candidate, body, incoming, ids))
            {
                Ok(upstream) => {
                    let completed = relay_external_stream(downstream, state, upstream)?;
                    if completed {
                        persist_protocol_observation(state, &candidate);
                    }
                    return Ok(());
                }
                Err(error) => {
                    if let Some(delay) = external_retry_delay(&error, attempt, &candidate) {
                        thread::sleep(delay);
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && protocol_fallback_allowed(error.status(), false, false)
                    {
                        continue 'candidate;
                    }
                    return Err(pre_output_router_error_response(&error));
                }
            }
        }
    }
    Err(json_error_response(
        503,
        status_text(503),
        "provider protocol is unsupported",
        Some("router_error"),
        &[],
    ))
}

fn serve_native_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let upstream = native_api::open_stream(
        state,
        route,
        config,
        body.as_object().expect("validated request object"),
        incoming,
        ids,
    )?;
    relay_native_stream(downstream, state, upstream).map(|_| ())
}

fn relay_native_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    mut upstream: NativeStream,
) -> Result<bool, Vec<u8>> {
    let response_headers = upstream.headers.clone();
    let mut monitor = DisconnectMonitor::start(downstream).ok();
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let polled = match monitor.as_mut() {
            Some(monitor) => state
                .backend
                .runtime
                .block_on(monitor.next_native_event(&mut upstream)),
            None => NativeStreamPoll::Event(state.backend.runtime.block_on(upstream.next_event())),
        };
        let event = match polled {
            NativeStreamPoll::Disconnected => return Ok(false),
            NativeStreamPoll::Event(Ok(Some(event))) => event,
            NativeStreamPoll::Event(Ok(None)) => return Ok(false),
            NativeStreamPoll::Event(Err(error)) if !started => {
                return Err(pre_output_router_error_response(&error));
            }
            NativeStreamPoll::Event(Err(error)) => {
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => return Ok(false),
                };
                let failure = stream_failure_value(&error, &response_id);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = write_stream_frames(downstream, &[frame]);
                }
                return Ok(false);
            }
        };
        let terminal = terminal_stream_event(&event.body);
        let completed = event.event == "response.completed";
        let failed = matches!(event.event.as_str(), "response.failed" | "error");
        let frame = event.frame;
        if started {
            if write_stream_frames(downstream, &[frame]).is_err() {
                return Ok(false);
            }
            if terminal {
                state.backend.runtime.block_on(upstream.finish());
                return Ok(completed);
            }
            continue;
        }
        if failed {
            if let Some(response) = pre_output_failure_response(&event.body) {
                return Err(response);
            }
            if write_stream_head_with_headers(downstream, &response_headers).is_err()
                || write_stream_frames(downstream, &[frame]).is_err()
            {
                return Ok(false);
            }
            return Ok(false);
        }
        let (output_emitted, tool_activity) = stream_event_activity(&event.body);
        pending_bytes = pending_bytes.saturating_add(frame.len());
        pending.push(frame);
        if pending.len() > MAX_PRE_OUTPUT_BUFFER_EVENTS
            || pending_bytes > MAX_PRE_OUTPUT_BUFFER_BYTES
        {
            return Err(json_error_response(
                502,
                status_text(502),
                "EMP could not parse the upstream response stream.",
                Some("pre_output_buffer_limit"),
                &[],
            ));
        }
        if output_emitted || tool_activity || terminal {
            if write_stream_head_with_headers(downstream, &response_headers).is_err()
                || write_stream_frames(downstream, &pending).is_err()
            {
                return Ok(false);
            }
            started = true;
            pending.clear();
            if terminal {
                state.backend.runtime.block_on(upstream.finish());
                return Ok(completed);
            }
        }
    }
}

fn relay_external_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    mut upstream: ExternalStream,
) -> Result<bool, Vec<u8>> {
    let mut monitor = DisconnectMonitor::start(downstream).ok();
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let polled = match monitor.as_mut() {
            Some(monitor) => state
                .backend
                .runtime
                .block_on(monitor.next_event(&mut upstream)),
            None => StreamPoll::Event(state.backend.runtime.block_on(upstream.next_event())),
        };
        let event = match polled {
            StreamPoll::Disconnected => return Ok(false),
            StreamPoll::Event(Ok(Some(event))) => event,
            StreamPoll::Event(Ok(None)) => return Ok(false),
            StreamPoll::Event(Err(error)) if !started => {
                return Err(pre_output_router_error_response(&error));
            }
            StreamPoll::Event(Err(error)) => {
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => return Ok(false),
                };
                let failure = stream_failure_value(&error, &response_id);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = write_stream_frames(downstream, &[frame]);
                }
                return Ok(false);
            }
        };
        let terminal = terminal_stream_event(&event.body);
        let completed =
            event.body.get("type").and_then(Value::as_str) == Some("response.completed");
        let failed = matches!(
            event.body.get("type").and_then(Value::as_str),
            Some("response.failed" | "error")
        );
        let frame = match sse_frame(&event.event, &event.body) {
            Ok(frame) => frame,
            Err(_) if !started => {
                return Err(json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                ));
            }
            Err(_) => return Ok(false),
        };
        if started {
            if write_stream_frames(downstream, &[frame]).is_err() {
                return Ok(false);
            }
            if terminal {
                return Ok(completed);
            }
            continue;
        }
        if failed {
            if let Some(response) = pre_output_failure_response(&event.body) {
                return Err(response);
            }
            if write_stream_head(downstream).is_err()
                || write_stream_frames(downstream, &[frame]).is_err()
            {
                return Ok(false);
            }
            return Ok(false);
        }
        let (output_emitted, tool_activity) = stream_event_activity(&event.body);
        pending_bytes = pending_bytes.saturating_add(frame.len());
        pending.push(frame);
        if pending.len() > MAX_PRE_OUTPUT_BUFFER_EVENTS
            || pending_bytes > MAX_PRE_OUTPUT_BUFFER_BYTES
        {
            return Err(json_error_response(
                502,
                status_text(502),
                "EMP could not parse the upstream response stream.",
                Some("pre_output_buffer_limit"),
                &[],
            ));
        }
        if output_emitted || tool_activity || terminal {
            if write_stream_head(downstream).is_err()
                || write_stream_frames(downstream, &pending).is_err()
            {
                return Ok(false);
            }
            started = true;
            pending.clear();
            if terminal {
                return Ok(completed);
            }
        }
    }
}

enum ResponsesRequestResult {
    Buffered(Vec<u8>),
    Streamed,
}

fn history_error_message(error: &HistoryError) -> &'static str {
    match error.reason() {
        "thread_missing" | "thread_identity_missing" => {
            "This task's local history is unavailable. For a Side chat, continue in the original task or start a new task."
        }
        "state_database_missing" | "database_missing" => {
            "Local Codex history was not found. Use the same Codex data directory as your client."
        }
        _ => "History reconstruction failed. Continue in the original task or start a new task.",
    }
}

fn history_error_detail(error: &HistoryError) -> Value {
    serde_json::json!({
        "type":"invalid_request_error",
        "code":"invalid_prompt",
        "message":history_error_message(error),
        "error_class":"history_reconstruction_failed",
        "reason":error.reason()
    })
}

fn history_http_error(error: &HistoryError) -> Vec<u8> {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code":"history_reconstruction_failed",
            "message":history_error_message(error),
            "error_class":"history_reconstruction_failed",
            "reason":error.reason()
        }
    }))
    .expect("history error is serializable");
    response("HTTP/1.1 409 Conflict", "application/json", &body, &[])
}

fn history_stream_error(error: &HistoryError) -> Value {
    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
    serde_json::json!({
        "type":"response.failed",
        "response":{
            "id":id,
            "object":"response",
            "status":"failed",
            "error":history_error_detail(error)
        }
    })
}

fn prepare_history(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<Value, HistoryError> {
    let reader = emp_codex::history::CodexHomeHistoryReader::new(&state.backend.codex_home);
    emp_history::prepare(
        body,
        incoming,
        route.dialect == emp_core::Dialect::CodexNative,
        &reader,
    )
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

enum DestinationPrepareError {
    Router(RouterError),
    History(&'static str),
    Context(emp_history::context::ContextAssessment),
}

fn prepare_destination_context(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<Value, DestinationPrepareError> {
    if route.dialect == emp_core::Dialect::CodexNative {
        return Ok(body.clone());
    }
    let protocol =
        protocol_candidates(route)
            .into_iter()
            .next()
            .ok_or(DestinationPrepareError::History(
                "history_compaction_failed",
            ))?;
    let candidate = route
        .with_protocol(protocol)
        .map_err(|_| DestinationPrepareError::History("history_compaction_failed"))?;
    let guard_body = if has_trailing_compaction_trigger(body) {
        compaction_summary_body(body)
    } else {
        body.clone()
    };
    let payload = project_external_payload(&candidate, &guard_body)
        .map_err(DestinationPrepareError::Router)?;
    let assessment = emp_history::context::assess(
        candidate.provider.value(),
        candidate.model.value(),
        candidate.protocol.as_config_str(),
        &payload,
    );
    if !assessment.blocked() {
        return Ok(body.clone());
    }
    let Some(safe_budget) = assessment.safe_input_limit else {
        return Err(DestinationPrepareError::Context(assessment));
    };
    let router = ExternalRouter::new(&state.backend.client);
    let mut summary_failure = None;
    let compacted = emp_history::context::compact_with(
        body,
        candidate.model.value(),
        safe_budget,
        |summary_body| {
            let ids = match projection_ids() {
                Ok(ids) => ids,
                Err(_) => return Err(()),
            };
            match state.backend.runtime.block_on(router.execute_complete(
                &candidate,
                summary_body,
                incoming,
                &ids,
            )) {
                Ok(result) => response_output_text(&result.body).ok_or(()),
                Err(error) => {
                    summary_failure = Some(error);
                    Err(())
                }
            }
        },
    )
    .map_err(|reason| {
        summary_failure.take().map_or(
            DestinationPrepareError::History(reason),
            DestinationPrepareError::Router,
        )
    })?;
    let final_guard_body = if has_trailing_compaction_trigger(&compacted) {
        compaction_summary_body(&compacted)
    } else {
        compacted.clone()
    };
    let payload = project_external_payload(&candidate, &final_guard_body)
        .map_err(DestinationPrepareError::Router)?;
    let final_assessment = emp_history::context::assess(
        candidate.provider.value(),
        candidate.model.value(),
        candidate.protocol.as_config_str(),
        &payload,
    );
    if final_assessment.blocked() {
        return Err(DestinationPrepareError::Context(final_assessment));
    }
    Ok(compacted)
}

fn destination_error_response(error: DestinationPrepareError) -> Vec<u8> {
    match error {
        DestinationPrepareError::Router(error) => router_error_response(error),
        DestinationPrepareError::History(reason) => history_http_error(&HistoryError::new(reason)),
        DestinationPrepareError::Context(assessment) => {
            let estimate = assessment
                .input_estimate
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            let limit = assessment
                .safe_input_limit
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
            json_error_response(
                413,
                status_text(413),
                &format!(
                    "context length exceeded: estimated input {estimate} tokens, safe input limit {limit}; next action: reduce input or use native remote compaction"
                ),
                Some("context_length_exceeded"),
                &[],
            )
        }
    }
}

fn has_trailing_compaction_trigger(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
        .and_then(Value::as_object)
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        == Some("compaction_trigger")
}

fn compaction_summary_body(body: &Value) -> Value {
    let mut input = match body.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(item)) => vec![Value::Object(item.clone())],
        Some(Value::String(text)) => vec![serde_json::json!({
            "type":"message",
            "role":"user",
            "content":[{"type":"input_text","text":text}]
        })],
        _ => Vec::new(),
    };
    input.retain(|item| item.get("type").and_then(Value::as_str) != Some("compaction_trigger"));
    input.push(serde_json::json!({
        "type":"message",
        "role":"user",
        "content":[{"type":"input_text","text":COMPACTION_PROMPT}]
    }));
    serde_json::json!({
        "model":body.get("model").cloned().unwrap_or(Value::Null),
        "input":input,
        "stream":false,
        "tools":[]
    })
}

fn response_output_text(value: &Value) -> Option<String> {
    if let Some(text) = value
        .get("output_text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_owned());
    }
    let mut parts = Vec::new();
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for part in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if matches!(
                part.get("type").and_then(Value::as_str),
                Some("output_text" | "text")
            ) && let Some(text) = part.get("text").and_then(Value::as_str)
            {
                parts.push(text);
            }
        }
    }
    let text = parts.join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn external_compaction_error(reason: &str) -> Vec<u8> {
    let message = format!("external_compaction_failed: reason={reason}");
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code":"external_compaction_failed",
            "type":"external_compaction_failed",
            "message":message,
            "failure_reason":reason
        }
    }))
    .expect("external compaction error is serializable");
    response("HTTP/1.1 502 Bad Gateway", "application/json", &body, &[])
}

fn external_compaction_response(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(Value, ResolvedRoute), Vec<u8>> {
    let summary_body = compaction_summary_body(body);
    let router = ExternalRouter::new(&state.backend.client);
    let candidates = protocol_candidates(route);
    for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(route_resolution_response)?;
        match state.backend.runtime.block_on(router.execute_complete(
            &candidate,
            &summary_body,
            incoming,
            ids,
        )) {
            Ok(result) => {
                let Some(summary) = response_output_text(&result.body) else {
                    return Err(external_compaction_error("summary_empty"));
                };
                if summary.chars().count() > MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS {
                    return Err(external_compaction_error("summary_too_large"));
                }
                let encoded = URL_SAFE.encode(summary.as_bytes());
                let item_id = random_hex(16)
                    .map(|value| format!("cmp_{value}"))
                    .map_err(|_| external_compaction_error("invalid_response"))?;
                let response_id = random_hex(16)
                    .map(|value| format!("resp_{value}"))
                    .map_err(|_| external_compaction_error("invalid_response"))?;
                let created_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default();
                return Ok((
                    serde_json::json!({
                        "id":response_id,
                        "object":"response",
                        "created_at":created_at,
                        "status":"completed",
                        "model":body.get("model").cloned().unwrap_or(Value::Null),
                        "output":[{
                            "id":item_id,
                            "type":"compaction",
                            "encrypted_content":format!("emp1:{encoded}")
                        }],
                        "usage":Value::Null
                    }),
                    candidate,
                ));
            }
            Err(error)
                if index + 1 < candidates.len()
                    && protocol_fallback_allowed(error.status(), false, false) =>
            {
                continue;
            }
            Err(error) => return Err(router_error_response(error)),
        }
    }
    Err(external_compaction_error("invalid_response"))
}

fn generated_response_stream(
    response_value: Value,
    ids: &ProjectionIds,
) -> Result<Vec<u8>, Vec<u8>> {
    let events =
        response_json_stream_events(response_value, ids, false).map_err(router_error_response)?;
    let mut output = Vec::new();
    for event in events {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        output.extend(sse_frame(event_type, &event).map_err(|_| {
            json_error_response(500, status_text(500), "internal server error", None, &[])
        })?);
    }
    Ok(output)
}

fn responses_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> ResponsesRequestResult {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        return ResponsesRequestResult::Buffered(json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        ));
    }
    let mut body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return ResponsesRequestResult::Buffered(body_error_response(error)),
    };
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return ResponsesRequestResult::Buffered(request_router_error_response(
            400,
            "request.model is required",
        ));
    };
    let mut config = match state.backend.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                500,
                status_text(500),
                "internal server error",
                None,
                &[],
            ));
        }
    };
    hydrate_provider_keys(&mut config, &state.backend.vault);
    if let Some(config) = config.as_object_mut() {
        config.insert(
            "_native_auth_path".to_owned(),
            Value::String(
                state
                    .backend
                    .native_auth_path
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    let route = match resolve_route(&config, model, |config, slug, account| {
        subscription_route_model(config, slug, account, |account| {
            account_catalog_headers(account, &state.backend.vault)
        })
    }) {
        Ok(route) => route,
        Err(error) => {
            return ResponsesRequestResult::Buffered(route_resolution_response(error));
        }
    };
    let ids = match projection_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                500,
                status_text(500),
                "internal server error",
                None,
                &[],
            ));
        }
    };
    let request_id = match random_hex(8) {
        Ok(value) => value,
        Err(_) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                500,
                status_text(500),
                "internal server error",
                None,
                &[],
            ));
        }
    };
    let mut incoming: BTreeMap<String, String> = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect();
    incoming.insert("X-EMP-Request-ID".to_owned(), request_id);
    body = match prepare_history(state, &route, &body, &incoming) {
        Ok(body) => body,
        Err(error) if python_truthy(body.get("stream")) => {
            let failed = history_stream_error(&error);
            let frame = match sse_frame("response.failed", &failed) {
                Ok(frame) => frame,
                Err(_) => {
                    return ResponsesRequestResult::Buffered(json_error_response(
                        500,
                        status_text(500),
                        "internal server error",
                        None,
                        &[],
                    ));
                }
            };
            let _ = write_stream_head(stream);
            let _ = write_stream_frames(stream, &[frame]);
            return ResponsesRequestResult::Streamed;
        }
        Err(error) => return ResponsesRequestResult::Buffered(history_http_error(&error)),
    };
    body = match prepare_destination_context(state, &route, &body, &incoming) {
        Ok(body) => body,
        Err(DestinationPrepareError::History(reason)) if python_truthy(body.get("stream")) => {
            let failed = history_stream_error(&HistoryError::new(reason));
            if let Ok(frame) = sse_frame("response.failed", &failed) {
                let _ = write_stream_head(stream);
                let _ = write_stream_frames(stream, &[frame]);
            }
            return ResponsesRequestResult::Streamed;
        }
        Err(error) => {
            return ResponsesRequestResult::Buffered(destination_error_response(error));
        }
    };
    if route.dialect != emp_core::Dialect::CodexNative && has_trailing_compaction_trigger(&body) {
        let (compacted, candidate) =
            match external_compaction_response(state, &route, &body, &incoming, &ids) {
                Ok(result) => result,
                Err(error) => return ResponsesRequestResult::Buffered(error),
            };
        persist_protocol_observation(state, &candidate);
        if python_truthy(body.get("stream")) {
            let stream_body = match generated_response_stream(compacted, &ids) {
                Ok(body) => body,
                Err(error) => return ResponsesRequestResult::Buffered(error),
            };
            if write_stream_head(stream).is_err()
                || write_stream_frames(stream, &[stream_body]).is_err()
            {
                return ResponsesRequestResult::Streamed;
            }
            return ResponsesRequestResult::Streamed;
        }
        let compacted = match serde_json::to_vec(&compacted) {
            Ok(body) => body,
            Err(_) => {
                return ResponsesRequestResult::Buffered(json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                ));
            }
        };
        return ResponsesRequestResult::Buffered(response(
            "HTTP/1.1 200 OK",
            "application/json",
            &compacted,
            &[],
        ));
    }
    if route.dialect == emp_core::Dialect::CodexNative {
        if python_truthy(body.get("stream")) {
            return match serve_native_stream(stream, state, &route, &config, &body, &incoming, &ids)
            {
                Ok(()) => ResponsesRequestResult::Streamed,
                Err(response) => ResponsesRequestResult::Buffered(response),
            };
        }
        return ResponsesRequestResult::Buffered(native_api::complete(
            state,
            &route,
            &config,
            body.as_object().expect("validated request object"),
            &incoming,
        ));
    }
    if python_truthy(body.get("stream")) {
        return match serve_external_stream(stream, state, &route, &body, &incoming, &ids) {
            Ok(()) => ResponsesRequestResult::Streamed,
            Err(response) => ResponsesRequestResult::Buffered(response),
        };
    }
    let router = ExternalRouter::new(&state.backend.client);
    let candidates = protocol_candidates(&route);
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = match route.with_protocol(protocol) {
            Ok(candidate) => candidate,
            Err(error) => {
                return ResponsesRequestResult::Buffered(route_resolution_response(error));
            }
        };
        for attempt in 0..2 {
            match state
                .backend
                .runtime
                .block_on(router.execute_complete(&candidate, &body, &incoming, &ids))
            {
                Ok(result) => {
                    let body = match serde_json::to_vec(&result.body) {
                        Ok(body) => body,
                        Err(_) => {
                            return ResponsesRequestResult::Buffered(json_error_response(
                                500,
                                status_text(500),
                                "internal server error",
                                None,
                                &[],
                            ));
                        }
                    };
                    persist_protocol_observation(state, &candidate);
                    return ResponsesRequestResult::Buffered(response(
                        &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
                        &result.content_type,
                        &body,
                        &[],
                    ));
                }
                Err(error) => {
                    if let Some(delay) = external_retry_delay(&error, attempt, &candidate) {
                        thread::sleep(delay);
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && protocol_fallback_allowed(error.status(), false, false)
                    {
                        continue 'candidate;
                    }
                    return ResponsesRequestResult::Buffered(router_error_response(error));
                }
            }
        }
    }
    ResponsesRequestResult::Buffered(json_error_response(
        503,
        status_text(503),
        "provider protocol is unsupported",
        Some("router_error"),
        &[],
    ))
}

fn compact_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        return json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        );
    }
    let mut body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let Some(model) = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return request_router_error_response(400, "request.model is required");
    };
    let mut config = match state.backend.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    hydrate_provider_keys(&mut config, &state.backend.vault);
    if let Some(config) = config.as_object_mut() {
        config.insert(
            "_native_auth_path".to_owned(),
            Value::String(
                state
                    .backend
                    .native_auth_path
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    let route = match resolve_route(&config, model, |config, slug, account| {
        subscription_route_model(config, slug, account, |account| {
            account_catalog_headers(account, &state.backend.vault)
        })
    }) {
        Ok(route) => route,
        Err(error) => return route_resolution_response(error),
    };
    let mut incoming = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    if let Ok(id) = random_hex(8) {
        incoming.insert("X-EMP-Request-ID".to_owned(), id);
    }
    body = match prepare_history(state, &route, &body, &incoming) {
        Ok(body) => body,
        Err(error) => return history_http_error(&error),
    };
    body = match prepare_destination_context(state, &route, &body, &incoming) {
        Ok(body) => body,
        Err(error) => return destination_error_response(error),
    };
    if route.dialect == emp_core::Dialect::CodexNative {
        return native_api::compact(
            state,
            &route,
            &config,
            body.as_object().expect("validated request object"),
            &incoming,
        );
    }
    let ids = match projection_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    let (compacted, candidate) =
        match external_compaction_response(state, &route, &body, &incoming, &ids) {
            Ok(result) => result,
            Err(error) => return error,
        };
    persist_protocol_observation(state, &candidate);
    match serde_json::to_vec(&compacted) {
        Ok(body) => response("HTTP/1.1 200 OK", "application/json", &body, &[]),
        Err(_) => json_error_response(500, status_text(500), "internal server error", None, &[]),
    }
}

fn native_search_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        return json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        );
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let config = match state.backend.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    if config
        .get("subscription_search")
        .and_then(Value::as_object)
        .and_then(|search| search.get("enabled"))
        .and_then(Value::as_bool)
        != Some(true)
    {
        return json_error_response(
            403,
            status_text(403),
            "Subscription web search is disabled",
            Some("router_error"),
            &[],
        );
    }
    let mut headers = BTreeMap::from([
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), format!("EMP/{VERSION}")),
    ]);
    let credentials = native_auth_document(&state.backend.native_auth_path)
        .and_then(|auth| account_auth_headers(&auth))
        .or_else(|| {
            config
                .get("accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|account| account.get("enabled") != Some(&Value::Bool(false)))
                .find_map(|account| {
                    let path = account.get("auth_file")?.as_str()?;
                    let auth = state
                        .backend
                        .vault
                        .read_encrypted_json(Path::new(path))
                        .ok()?;
                    account_auth_headers(&auth)
                })
        });
    if let Some(credentials) = credentials {
        headers.extend(credentials);
    } else {
        for name in ["authorization", "chatgpt-account-id"] {
            if let Some(value) = request.header(name) {
                headers.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    if let Ok(id) = random_hex(8) {
        headers.insert("X-EMP-Request-ID".to_owned(), id);
    }
    let base = config
        .get("codex_base_url")
        .and_then(Value::as_str)
        .unwrap_or("https://chatgpt.com/backend-api/codex")
        .trim_end_matches('/');
    let endpoint = if base.ends_with("/alpha/search") {
        base.to_owned()
    } else {
        format!("{base}/alpha/search")
    };
    let encoded = match serde_json::to_vec(&body) {
        Ok(body) => body,
        Err(_) => return request_router_error_response(400, "request body is invalid"),
    };
    let upstream = match state.backend.runtime.block_on(state.backend.client.open(
        HttpMethod::Post,
        &endpoint,
        headers,
        Some(encoded),
        false,
    )) {
        Ok(response) => response,
        Err(_) => {
            return json_error_response(
                503,
                status_text(503),
                "Cannot connect to subscription web search",
                Some("network_error"),
                &[],
            );
        }
    };
    let status = upstream.status();
    let content_type = upstream
        .header("content-type")
        .unwrap_or("application/json")
        .to_owned();
    let raw = match state
        .backend
        .runtime
        .block_on(upstream.read_limited(64 * 1024 * 1024))
    {
        Ok(raw) => raw,
        Err(_) => {
            return json_error_response(
                502,
                status_text(502),
                "native search response is too large or incomplete",
                Some("protocol_error"),
                &[],
            );
        }
    };
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        &content_type,
        &raw,
        &[],
    )
}

struct QuotaEventSlot<'a> {
    active: &'a AtomicUsize,
}

impl QuotaEventSlot<'_> {
    fn acquire(active: &AtomicUsize) -> Option<QuotaEventSlot<'_>> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < QUOTA_EVENT_SLOT_LIMIT).then_some(count + 1)
            })
            .ok()
            .map(|_| QuotaEventSlot { active })
    }
}

impl Drop for QuotaEventSlot<'_> {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_quota_events(stream: &mut TcpStream, request: Request<'_>, state: &ServerState, now: f64) {
    if !same_origin(request, state.port) {
        let _ = stream.write_all(&cross_origin_response("management session is required"));
        let _ = stream.flush();
        return;
    }
    let cookie = request.session_cookie();
    if !state.sessions.contains(cookie.as_deref(), now) {
        let _ = stream.write_all(&unauthorized_response());
        let _ = stream.flush();
        return;
    }
    let Some(_slot) = QuotaEventSlot::acquire(&state.backend.quota_event_slots) else {
        let response = json_error_response(
            503,
            status_text(503),
            "Too many quota subscribers",
            None,
            &[("Retry-After", "15")],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    };
    if stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
        || stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\nConnection: close\r\n\r\n",
            )
            .is_err()
        || stream.flush().is_err()
    {
        return;
    }
    let mut observed_revision = u64::MAX;
    loop {
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        let revision = match state.backend.quota_revision.lock() {
            Ok(revision) => revision,
            Err(_) => break,
        };
        let (revision, _) = match state.backend.quota_condition.wait_timeout_while(
            revision,
            QUOTA_EVENT_KEEP_ALIVE,
            |revision| *revision == observed_revision && !state.shutdown.load(Ordering::Acquire),
        ) {
            Ok(result) => result,
            Err(_) => break,
        };
        let current = *revision;
        drop(revision);
        if state.shutdown.load(Ordering::Acquire)
            || !state.sessions.contains(cookie.as_deref(), system_now())
        {
            break;
        }
        let frame: &[u8] = if current != observed_revision {
            b"event: quota-updated\ndata: {}\n\n"
        } else {
            b": keep-alive\n\n"
        };
        observed_revision = current;
        if stream.write_all(frame).is_err() || stream.flush().is_err() {
            break;
        }
    }
}

fn websocket_router_error(error: &RouterError) -> Value {
    let failure = stream_failure_value(error, "resp_websocket_error");
    serde_json::json!({
        "type":"error", "status":error.status(),
        "error":failure["response"]["error"]
    })
}

fn native_stream_error_value(
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<&str>,
    response_id: &str,
) -> Value {
    let mut error = serde_json::json!({
        "code":stream_error_code(error_class),
        "message":format!("HTTP {status}: {}",public_failure_message(error_class,failure_reason,status)),
        "status":status,"error_class":error_class.as_str()
    });
    if let Some(reason) = failure_reason {
        error["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    serde_json::json!({"type":"response.failed","response":{"id":response_id,"object":"response","status":"failed","error":error}})
}

fn native_websocket_identity_headers(
    headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "chatgpt-account-id"
            )
        })
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect()
}

struct NativeUpstreamConnection {
    url: String,
    proxy: Option<String>,
    identity: BTreeMap<String, String>,
    client: ClientWebSocket,
}

fn serve_responses_websocket(
    stream: &mut TcpStream,
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        let response = json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    }
    let connection_tokens = request
        .header("Connection")
        .unwrap_or_default()
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if request
        .header("Upgrade")
        .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
        || !connection_tokens.iter().any(|value| value == "upgrade")
        || request.header("Sec-WebSocket-Version") != Some("13")
    {
        let response = json_error_response(
            400,
            status_text(400),
            "invalid websocket upgrade",
            None,
            &[],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    }
    let accept = match websocket_accept(request.header("Sec-WebSocket-Key").unwrap_or_default()) {
        Ok(value) => value,
        Err(error) => {
            let response =
                json_error_response(400, status_text(400), &error.to_string(), None, &[]);
            let _ = stream.write_all(&response);
            let _ = stream.flush();
            return;
        }
    };
    let incoming = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let mut websocket = WebSocketConnection::new(stream);
    let mut native_upstream: Option<NativeUpstreamConnection> = None;
    let mut last_native_response_id: Option<String> = None;
    loop {
        let text = match websocket.receive_text() {
            Ok(Some(value)) => value,
            Ok(None) => return,
            Err(error) => {
                websocket.close(error.close_code(), &error.to_string());
                return;
            }
        };
        let mut request_body = match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(value)) => value,
            _ => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":400,"error":{"code":"invalid_request","message":"websocket request must be a JSON object"}}));
                continue;
            }
        };
        if request_body
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .as_deref()
            != Some("response.create")
        {
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":400,"error":{"code":"invalid_request","message":"websocket request.type must be response.create"}}));
            continue;
        }
        let etag = catalog_api::response_catalog_etag(state).unwrap_or_default();
        if websocket.send_json(&serde_json::json!({"type":"codex.response.metadata","headers":{"x-models-etag":etag}})).is_err(){return;}
        let Some(model) = request_body
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            let _=websocket.send_json(&serde_json::json!({"type":"error","status":400,"error":{"code":"invalid_request","message":"request.model is required"}}));
            continue;
        };
        let mut config = match state.backend.config.lock() {
            Ok(config) => config.clone(),
            Err(_) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":500,"error":{"code":"internal_error","message":"internal server error"}}));
                continue;
            }
        };
        hydrate_provider_keys(&mut config, &state.backend.vault);
        if let Some(config) = config.as_object_mut() {
            config.insert(
                "_native_auth_path".to_owned(),
                Value::String(
                    state
                        .backend
                        .native_auth_path
                        .to_string_lossy()
                        .into_owned(),
                ),
            );
        }
        let route = match resolve_route(&config, model, |config, slug, account| {
            subscription_route_model(config, slug, account, |account| {
                account_catalog_headers(account, &state.backend.vault)
            })
        }) {
            Ok(route) => route,
            Err(error) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status(),"error":{"code":"router_error","message":error.to_string()}}));
                continue;
            }
        };
        let ids = match projection_ids() {
            Ok(ids) => ids,
            Err(_) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":500,"error":{"code":"internal_error","message":"internal server error"}}));
                continue;
            }
        };
        let generate = request_body
            .remove("generate")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if !generate {
            let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
            let usage = serde_json::json!({"input_tokens":0,"input_tokens_details":Value::Null,"output_tokens":0,"output_tokens_details":Value::Null,"total_tokens":0});
            if websocket
                .send_json(&serde_json::json!({"type":"response.created","response":{"id":id}}))
                .is_err()
            {
                return;
            }
            if websocket.send_json(&serde_json::json!({"type":"response.completed","response":{"id":id,"object":"response","status":"completed","output":[],"usage":usage}})).is_err(){return;}
            continue;
        }
        let mut request_headers = incoming.clone();
        if let Ok(id) = random_hex(8) {
            request_headers.insert("X-EMP-Request-ID".to_owned(), id);
        }
        request_body = match prepare_history(
            state,
            &route,
            &Value::Object(request_body),
            &request_headers,
        ) {
            Ok(Value::Object(body)) => body,
            Ok(_) => {
                let error = HistoryError::new("invalid_history_projection");
                if websocket.send_json(&history_stream_error(&error)).is_err() {
                    return;
                }
                continue;
            }
            Err(error) => {
                if websocket.send_json(&history_stream_error(&error)).is_err() {
                    return;
                }
                continue;
            }
        };
        request_body = match prepare_destination_context(
            state,
            &route,
            &Value::Object(request_body.clone()),
            &request_headers,
        ) {
            Ok(Value::Object(body)) => body,
            Ok(_) => {
                let error = HistoryError::new("invalid_history_projection");
                if websocket.send_json(&history_stream_error(&error)).is_err() {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Router(error)) => {
                if websocket
                    .send_json(&websocket_router_error(&error))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::History(reason)) => {
                if websocket
                    .send_json(&history_stream_error(&HistoryError::new(reason)))
                    .is_err()
                {
                    return;
                }
                continue;
            }
            Err(DestinationPrepareError::Context(_)) => {
                let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                let failed = native_stream_error_value(
                    413,
                    FailureClass::ContextLengthExceeded,
                    Some("context_length_exceeded"),
                    &id,
                );
                if websocket.send_json(&failed).is_err() {
                    return;
                }
                continue;
            }
        };
        if route.dialect == emp_core::Dialect::CodexNative {
            let plan = match native_api::websocket_plan(
                state,
                &route,
                &config,
                &request_body,
                &request_headers,
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
                    continue;
                }
            };
            let previous = request_body
                .get("previous_response_id")
                .and_then(Value::as_str);
            let plan_identity = native_websocket_identity_headers(&plan.headers);
            let proxy = state.backend.client.websocket_proxy_for(&plan.url);
            let route_matches = native_upstream.as_ref().is_some_and(|upstream| {
                upstream.url == plan.url
                    && proxy.as_ref().is_ok_and(|proxy| proxy == &upstream.proxy)
                    && upstream.identity == plan_identity
            });
            if previous
                .is_some_and(|id| !route_matches || last_native_response_id.as_deref() != Some(id))
            {
                last_native_response_id = None;
                let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                continue;
            }
            if !route_matches {
                native_upstream = None;
                last_native_response_id = None;
            }
            let mut connected_now = false;
            if native_upstream.is_none()
                && let Ok(selected_proxy) = &proxy
                && let Ok(client) = ClientWebSocket::connect_with_proxy(
                    &plan.url,
                    &plan.headers,
                    Duration::from_secs(15),
                    selected_proxy.as_deref(),
                )
            {
                native_upstream = Some(NativeUpstreamConnection {
                    url: plan.url.clone(),
                    proxy: selected_proxy.clone(),
                    identity: plan_identity.clone(),
                    client,
                });
                connected_now = true;
            }
            if let Some(upstream) = native_upstream.as_mut() {
                let client = &mut upstream.client;
                if connected_now {
                    let selected = native_response_headers(
                        &serde_json::json!({"headers":client.response_headers()}),
                        &plan.requested_model,
                        &plan.upstream_model,
                    );
                    let state_headers = selected
                        .into_iter()
                        .filter(|(name, _)| !name.eq_ignore_ascii_case("x-models-etag"))
                        .collect::<serde_json::Map<_, _>>();
                    if !state_headers.is_empty()
                        && websocket
                            .send_json(&serde_json::json!({"type":"response.metadata","headers":state_headers}))
                            .is_err()
                    {
                        return;
                    }
                }
                if client.send_json(&plan.payload).is_err() {
                    native_upstream = None;
                    last_native_response_id = None;
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let error =
                        native_stream_error_value(502, FailureClass::Network, Some("network"), &id);
                    let _ = websocket.send_json(&error);
                    continue;
                }
                let mut terminal = false;
                let mut completed_id = None;
                let mut projected_error = false;
                while let Ok(Some(event)) = client.receive_json() {
                    let event = match plan.project_event(&event) {
                        Ok(event) => event,
                        Err(error) => {
                            let _ = websocket.send_json(&serde_json::json!({
                                "type":"error",
                                "status":error.status,
                                "error":error.body["error"]
                            }));
                            projected_error = true;
                            break;
                        }
                    };
                    if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                        completed_id = event
                            .get("response")
                            .and_then(|response| response.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                    }
                    terminal = terminal_stream_event(&event);
                    if websocket.send_json(&event).is_err() {
                        return;
                    }
                    if terminal {
                        break;
                    }
                }
                if terminal {
                    last_native_response_id = completed_id;
                    continue;
                }
                native_upstream = None;
                last_native_response_id = None;
                if projected_error {
                    continue;
                }
                if previous.is_some() {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                } else {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let error = native_stream_error_value(
                        502,
                        FailureClass::StreamIncomplete,
                        Some("stream_incomplete"),
                        &id,
                    );
                    let _ = websocket.send_json(&error);
                }
                continue;
            }
            if previous.is_some() {
                let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                continue;
            }
        }
        request_body.insert("stream".to_owned(), Value::Bool(true));
        let mut sent_output = false;
        if route.dialect == emp_core::Dialect::CodexNative {
            let mut upstream = match native_api::open_stream_result(
                state,
                &route,
                &config,
                &request_body,
                &request_headers,
                &ids,
            ) {
                Ok(stream) => stream,
                Err(error) => {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
                    continue;
                }
            };
            let state_headers = upstream
                .headers
                .iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case("x-models-etag"))
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect::<serde_json::Map<_, _>>();
            if !state_headers.is_empty()
                && websocket
                    .send_json(
                        &serde_json::json!({"type":"response.metadata","headers":state_headers}),
                    )
                    .is_err()
            {
                return;
            }
            loop {
                match state.backend.runtime.block_on(upstream.next_event()) {
                    Ok(Some(event)) => {
                        sent_output |= stream_event_activity(&event.body).0;
                        if websocket.send_json(&event.body).is_err() {
                            return;
                        }
                        if terminal_stream_event(&event.body) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if sent_output {
                            let id = format!(
                                "resp_{}",
                                random_hex(16).unwrap_or_else(|_| "0".repeat(32))
                            );
                            let failure = stream_failure_value(&error, &id);
                            let _ = websocket.send_json(&failure);
                        } else {
                            let _ = websocket.send_json(&websocket_router_error(&error));
                        }
                        break;
                    }
                }
            }
        } else {
            let router = ExternalRouter::new(&state.backend.client);
            let mut upstream = match state.backend.runtime.block_on(router.open_stream(
                &route,
                &Value::Object(request_body.clone()),
                &request_headers,
                &ids,
            )) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = websocket.send_json(&websocket_router_error(&error));
                    continue;
                }
            };
            loop {
                match state.backend.runtime.block_on(upstream.next_event()) {
                    Ok(Some(event)) => {
                        sent_output |= stream_event_activity(&event.body).0;
                        if websocket.send_json(&event.body).is_err() {
                            return;
                        }
                        if terminal_stream_event(&event.body) {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if sent_output {
                            let id = format!(
                                "resp_{}",
                                random_hex(16).unwrap_or_else(|_| "0".repeat(32))
                            );
                            let failure = stream_failure_value(&error, &id);
                            let _ = websocket.send_json(&failure);
                        } else {
                            let _ = websocket.send_json(&websocket_router_error(&error));
                        }
                        break;
                    }
                }
            }
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &ServerState) {
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let raw = match read_request_head(&mut stream) {
        Some(raw) => raw,
        None => {
            let _ = stream.write_all(&bad_request_response());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    let response = match parse_request(&raw.head) {
        Some(request)
            if request.method == RequestMethod::Get
                && request.raw_path() == "/v1/responses"
                && request
                    .header("Upgrade")
                    .is_some_and(|value| value.eq_ignore_ascii_case("websocket")) =>
        {
            serve_responses_websocket(&mut stream, request, state, system_now());
            None
        }
        Some(request)
            if request.method == RequestMethod::Post
                && request.raw_path() == "/v1/alpha/search" =>
        {
            Some(native_search_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                system_now(),
            ))
        }
        Some(request)
            if request.method == RequestMethod::Post
                && request.raw_path() == "/api/accounts/import" =>
        {
            Some(management_account_import_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                system_now(),
            ))
        }
        Some(request)
            if request.method == RequestMethod::Post
                && matches!(
                    request.raw_path(),
                    "/api/migration/export" | "/api/migration/import"
                ) =>
        {
            Some(management_migration_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                system_now(),
            ))
        }
        Some(request)
            if request.method == RequestMethod::Post
                && matches!(
                    request.raw_path(),
                    "/api/providers/discover" | "/api/catalog/refresh" | "/api/config"
                ) =>
        {
            Some(catalog_api::management_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                system_now(),
            ))
        }
        Some(request)
            if request.method == RequestMethod::Post
                && request.raw_path() == "/v1/responses/compact" =>
        {
            Some(compact_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                system_now(),
            ))
        }
        Some(request)
            if request.method == RequestMethod::Post && request.raw_path() == "/v1/responses" =>
        {
            match responses_request(&mut stream, request, raw.body_prefix, state, system_now()) {
                ResponsesRequestResult::Buffered(response) => Some(response),
                ResponsesRequestResult::Streamed => None,
            }
        }
        Some(request)
            if request.method == RequestMethod::Get
                && request.raw_path() == "/api/accounts/events" =>
        {
            serve_quota_events(&mut stream, request, state, system_now());
            None
        }
        Some(request)
            if request.method == RequestMethod::Post
                && request.raw_path().starts_with("/api/accounts/")
                && (request.raw_path().ends_with("/quota")
                    || request.raw_path().ends_with("/quota-reset")) =>
        {
            Some(management_quota_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                system_now(),
            ))
        }
        Some(request) => Some(route_request(request, state)),
        None => Some(bad_request_response()),
    };
    if let Some(response) = response {
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }
    let _ = stream.shutdown(Shutdown::Write);
}

fn regular_file(path: &Path) -> bool {
    fs_metadata(path)
        .is_some_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn fs_metadata(path: &Path) -> Option<std::fs::Metadata> {
    std::fs::symlink_metadata(path).ok()
}

fn native_account_snapshot(state: &ServerState, config: &Value) -> Value {
    let quota = state
        .backend
        .native_quota
        .lock()
        .ok()
        .and_then(|quota| quota.clone())
        .unwrap_or(Value::Null);
    serde_json::json!({
        "id": "@native",
        "name": "Current Codex login",
        "prefix": "",
        "native": true,
        "credential_set": regular_file(&state.backend.native_auth_path),
        "hidden_models": config
            .get("native_hidden_models")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "model_context_windows": config
            .get("native_model_context_windows")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new())),
        "quota": quota,
    })
}

fn accounts_snapshot(state: &ServerState) -> Option<Value> {
    let config = state.backend.config.lock().ok()?.clone();
    let duplicates = catalog_api::duplicate_accounts(
        &config,
        &state.backend.vault,
        &state.backend.native_auth_path,
    );
    let public = public_configuration_with_file_status(&config, &duplicates, regular_file).ok()?;
    let errors = state
        .backend
        .quota_refresh_errors
        .lock()
        .ok()
        .map(|errors| {
            errors
                .iter()
                .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                .collect::<serde_json::Map<_, _>>()
        })?;
    Some(serde_json::json!({
        "native_account": native_account_snapshot(state, &config),
        "accounts": public
            .get("accounts")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new())),
        "refresh_errors": errors,
    }))
}

fn account_public_snapshot(state: &ServerState, account_id: &str) -> Option<Value> {
    accounts_snapshot(state)?
        .get("accounts")?
        .as_array()?
        .iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
        .cloned()
}

fn native_auth_document(path: &Path) -> Option<Value> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_NATIVE_AUTH_BYTES as u64
    {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
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
        .config
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    let Some(account) = config
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
    account["credential_status"] = Value::String(status.to_owned());
    if let Some(quota) = quota {
        account["quota"] = quota.clone();
    }
    save_configuration(
        &config,
        Some(&state.backend.config_path),
        &state.backend.vault,
    )
    .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    *config = load_configuration(Some(&state.backend.config_path))
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    drop(config);
    account_public_snapshot(state, account_id)
        .ok_or_else(|| QuotaError::new("account changed during quota refresh", "quota_error"))
}

fn quota_owner_key(state: &ServerState, account_id: &str) -> Result<String, QuotaError> {
    if account_id == "@native" {
        return Ok(account_id.to_owned());
    }
    let accounts = state
        .backend
        .config
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !accounts
        .iter()
        .any(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
    {
        return Err(QuotaError::new(
            format!("unknown account: {account_id}"),
            "quota_error",
        ));
    }
    let mut owners = Vec::<(String, Value)>::new();
    if let Some(native) = native_auth_document(&state.backend.native_auth_path) {
        owners.push(("@native".to_owned(), native));
    }
    for account in accounts {
        let Some(id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        let auth = account
            .get("auth_file")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
            .and_then(|path| {
                state
                    .backend
                    .vault
                    .read_encrypted_json(Path::new(path))
                    .ok()
            });
        let source = auth.as_ref().and_then(|auth| {
            owners
                .iter()
                .find(|(_, seen)| same_account_auth(auth, seen))
                .map(|(owner, _)| owner.clone())
        });
        if id == account_id {
            return Ok(source.unwrap_or_else(|| id.to_owned()));
        }
        if source.is_none()
            && let Some(auth) = auth
        {
            owners.push((id.to_owned(), auth));
        }
    }
    Ok(account_id.to_owned())
}

fn record_quota_snapshot(state: &ServerState, account_id: &str, quota: &Value) {
    let Ok(owner) = quota_owner_key(state, account_id) else {
        return;
    };
    let _ = state
        .backend
        .quota_history
        .append_snapshot(&owner, quota, system_now().trunc() as i64);
}

fn refresh_imported_account(state: &ServerState, account_id: &str) -> Result<Value, QuotaError> {
    let target = state
        .backend
        .config
        .lock()
        .ok()
        .and_then(|config| {
            config
                .get("accounts")?
                .as_array()?
                .iter()
                .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
                .cloned()
        })
        .ok_or_else(|| QuotaError::new(format!("unknown account: {account_id}"), "quota_error"))?;
    let auth_file = target
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| QuotaError::new("account credentials are not configured", "quota_error"))?
        .to_owned();
    let auth_path = Path::new(&auth_file);
    let read_auth = || {
        state
            .backend
            .vault
            .read_encrypted_json(auth_path)
            .map_err(|_| QuotaError::new("stored encrypted auth.json is invalid", "quota_error"))
    };
    let query = |auth: &Value, allow_refresh: bool| {
        run_quota_query_persisting(
            auth,
            &state.backend.codex_binary,
            Duration::from_secs(45),
            allow_refresh,
            |refreshed| {
                state
                    .backend
                    .vault
                    .write_encrypted_json(auth_path, refreshed)
                    .map_err(|_| ())
            },
        )
    };
    let auth = read_auth()?;
    if native_auth_document(&state.backend.native_auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return match read_native_login_quota(
            &state.backend.native_auth_path,
            &state.backend.codex_binary,
            Duration::from_secs(45),
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
        Err(error) if error.code() == "quota_auth_required" => {
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
        &state.backend.native_auth_path,
        &state.backend.codex_binary,
        Duration::from_secs(45),
    )?;
    if let Ok(mut current) = state.backend.native_quota.lock() {
        *current = Some(quota.clone());
    } else {
        return Err(QuotaError::new(
            "Codex account quota check failed",
            "quota_error",
        ));
    }
    let config = state
        .backend
        .config
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .clone();
    record_quota_snapshot(state, account_id, &quota);
    Ok(native_account_snapshot(state, &config))
}

fn notify_quota_update(state: &ServerState, account_id: &str, error: Option<&str>) {
    if let Ok(mut errors) = state.backend.quota_refresh_errors.lock() {
        match error {
            Some(error) => {
                errors.insert(account_id.to_owned(), error.to_owned());
            }
            None => {
                errors.remove(account_id);
            }
        }
    }
    if let Ok(mut revision) = state.backend.quota_revision.lock() {
        *revision = revision.wrapping_add(1);
        state.backend.quota_condition.notify_all();
    }
}

fn refresh_account_by_id(state: &ServerState, account_id: &str) -> Result<Value, QuotaError> {
    let result = refresh_account_by_id_inner(state, account_id);
    notify_quota_update(
        state,
        account_id,
        result.as_ref().err().map(|error| error.code()),
    );
    result
}

fn quota_history_response(
    state: &ServerState,
    account_id: &str,
    range_name: &str,
    now: i64,
) -> Result<Value, QuotaHistoryResponseError> {
    let owner = quota_owner_key(state, account_id).map_err(QuotaHistoryResponseError::Account)?;
    let mut result = state
        .backend
        .quota_history
        .query(&owner, range_name, now)
        .map_err(QuotaHistoryResponseError::History)?;
    result["account_id"] = Value::String(account_id.to_owned());
    Ok(result)
}

enum QuotaHistoryResponseError {
    Account(QuotaError),
    History(QuotaHistoryError),
}

fn consume_quota_reset_for_account(
    state: &ServerState,
    account_id: &str,
    idempotency_key: &str,
) -> Result<String, QuotaError> {
    if account_id == "@native" {
        return consume_native_quota_reset(
            &state.backend.native_auth_path,
            &state.backend.codex_binary,
            Duration::from_secs(45),
            idempotency_key,
        );
    }
    let target = state
        .backend
        .config
        .lock()
        .ok()
        .and_then(|config| {
            config
                .get("accounts")?
                .as_array()?
                .iter()
                .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
                .cloned()
        })
        .ok_or_else(|| QuotaError::new(format!("unknown account: {account_id}"), "quota_error"))?;
    let auth_file = target
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| QuotaError::new("account credentials are not configured", "quota_error"))?
        .to_owned();
    let auth_path = Path::new(&auth_file);
    let read_auth = || {
        state
            .backend
            .vault
            .read_encrypted_json(auth_path)
            .map_err(|_| QuotaError::new("stored encrypted auth.json is invalid", "quota_error"))
    };
    let auth = read_auth()?;
    if native_auth_document(&state.backend.native_auth_path)
        .is_some_and(|native| same_account_auth(&auth, &native))
    {
        return consume_native_quota_reset(
            &state.backend.native_auth_path,
            &state.backend.codex_binary,
            Duration::from_secs(45),
            idempotency_key,
        );
    }
    let query = |auth: &Value, allow_refresh: bool| {
        run_quota_reset_persisting(
            auth,
            &state.backend.codex_binary,
            Duration::from_secs(45),
            allow_refresh,
            idempotency_key,
            |refreshed| {
                state
                    .backend
                    .vault
                    .write_encrypted_json(auth_path, refreshed)
                    .map_err(|_| ())
            },
        )
    };
    match query(&auth, false) {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.code() == "quota_auth_required" => query(&read_auth()?, true),
        Err(error) => Err(error),
    }
}

fn management_quota_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    let supplied_cookie = request.session_cookie();
    if !state.sessions.contains(supplied_cookie.as_deref(), now) {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let path = request.raw_path();
    let reset = path.ends_with("/quota-reset");
    let suffix = if reset { "/quota-reset" } else { "/quota" };
    let raw_account = &path["/api/accounts/".len()..path.len() - suffix.len()];
    let account = percent_decode(raw_account.trim_end_matches('/'), false);
    let known_account = account == "@native"
        || state.backend.config.lock().is_ok_and(|config| {
            config
                .get("accounts")
                .and_then(Value::as_array)
                .is_some_and(|accounts| {
                    accounts.iter().any(|candidate| {
                        candidate.get("id").and_then(Value::as_str) == Some(account.as_str())
                    })
                })
        });
    if !known_account {
        return json_error_response(
            503,
            status_text(503),
            &format!("unknown account: {account}"),
            Some("quota_error"),
            &[],
        );
    }
    let refresh_lock = match state.backend.quota_refresh_locks.lock() {
        Ok(mut locks) => Arc::clone(
            locks
                .entry(account.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        ),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    let _refresh_guard = match refresh_lock.lock() {
        Ok(guard) => guard,
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    if reset {
        let key = body
            .get("idempotency_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let outcome = match consume_quota_reset_for_account(state, &account, key) {
            Ok(outcome) => outcome,
            Err(error) => {
                let status = if error.code() == "quota_reset_invalid_request" {
                    400
                } else {
                    503
                };
                return json_error_response(
                    status,
                    status_text(status),
                    &error.to_string(),
                    Some(error.code()),
                    &[],
                );
            }
        };
        let (account_snapshot, refresh_error) = match refresh_account_by_id(state, &account) {
            Ok(snapshot) => (snapshot, Value::Null),
            Err(error) => (
                Value::Null,
                serde_json::json!({
                    "code": error.code(),
                    "message": error.to_string(),
                }),
            ),
        };
        let response_body = serde_json::to_vec(&serde_json::json!({
            "outcome": outcome,
            "account": account_snapshot,
            "refresh_error": refresh_error,
        }))
        .expect("quota reset result is serializable");
        return response("HTTP/1.1 200 OK", "application/json", &response_body, &[]);
    }
    let refreshed = refresh_account_by_id(state, &account);
    match refreshed {
        Ok(account_snapshot) => {
            let body = serde_json::to_vec(&serde_json::json!({"account": account_snapshot}))
                .expect("account snapshot is serializable");
            response("HTTP/1.1 200 OK", "application/json", &body, &[])
        }
        Err(error) => json_error_response(
            503,
            status_text(503),
            &error.to_string(),
            Some(error.code()),
            &[],
        ),
    }
}

fn management_migration_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let password = body
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if request.raw_path() == "/api/migration/export" {
        let group_values = body.get("groups").and_then(Value::as_array);
        let groups = match group_values {
            Some(values) => {
                let Some(values) = values.iter().map(Value::as_str).collect::<Option<Vec<_>>>()
                else {
                    return json_error_response(
                        400,
                        status_text(400),
                        "select at least one valid export category",
                        None,
                        &[],
                    );
                };
                match ExportGroups::from_list(&values) {
                    Ok(groups) => Some(groups),
                    Err(error) => {
                        return json_error_response(
                            400,
                            status_text(400),
                            &error.to_string(),
                            None,
                            &[],
                        );
                    }
                }
            }
            None => None,
        };
        let config = match state.backend.config.lock() {
            Ok(config) => config.clone(),
            Err(_) => {
                return json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                );
            }
        };
        let (bundle, summary) = match export_migration_bundle_with_summary(
            &config,
            password,
            &state.backend.vault,
            groups.as_ref(),
            Some(&state.backend.native_auth_path),
        ) {
            Ok(result) => result,
            Err(error) => {
                return json_error_response(400, status_text(400), &error.to_string(), None, &[]);
            }
        };
        let summary = serde_json::json!({
            "accounts":summary.accounts,
            "providers":summary.providers,
            "models":summary.models,
            "groups":summary.groups,
            "native_login_included":summary.native_login_included,
            "native_login_missing":summary.native_login_missing,
        })
        .to_string();
        return response(
            "HTTP/1.1 200 OK",
            "application/octet-stream",
            &bundle,
            &[
                ("Cache-Control", "no-store"),
                ("Content-Disposition", "attachment; filename=\"EMP.emp\""),
                ("X-EMP-Export-Summary", &summary),
            ],
        );
    }
    let Some(encoded) = body
        .get("bundle")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return json_error_response(
            400,
            status_text(400),
            "migration bundle is required",
            None,
            &[],
        );
    };
    let bundle = match STANDARD.decode(encoded) {
        Ok(bundle) => bundle,
        Err(_) => {
            return json_error_response(
                400,
                status_text(400),
                "migration bundle is not valid base64",
                None,
                &[],
            );
        }
    };
    let current = match state.backend.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    let (updated, summary) = match import_migration_bundle(
        &current,
        &bundle,
        password,
        &state.backend.config_path,
        &state.backend.vault,
    ) {
        Ok(result) => result,
        Err(error) => {
            return json_error_response(400, status_text(400), &error.to_string(), None, &[]);
        }
    };
    match state.backend.config.lock() {
        Ok(mut config) => *config = updated,
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "status":"ok",
        "accounts":summary.accounts,
        "providers":summary.providers,
        "models":summary.models,
        "renamed_accounts":summary.renamed_accounts,
    }))
    .expect("migration response is serializable");
    response("HTTP/1.1 200 OK", "application/json", &body, &[])
}

fn import_account_state(state: &ServerState, body: &Value) -> Result<Value, String> {
    let metadata = body
        .as_object()
        .ok_or_else(|| "account import body must be an object".to_owned())?;
    let auth = metadata
        .get("auth_json")
        .ok_or_else(|| "auth_json must be a JSON object".to_owned())?;
    let auth = validate_auth_json(auth).map_err(|error| error.to_string())?;
    let current = state
        .backend
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())?
        .clone();
    let account_id = metadata
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "account.id must be a safe single path segment".to_owned())?;
    let auth_path = emp_state::account_auth_path(&current, account_id, &state.backend.config_path)
        .map_err(|error| error.to_string())?;
    let raw = serde_json::json!({
        "id":metadata.get("id").cloned().unwrap_or(Value::Null),
        "name":metadata.get("name").cloned().unwrap_or_else(|| Value::String(account_id.to_owned())),
        "prefix":metadata.get("prefix").cloned().unwrap_or(Value::Null),
        "auth_file":auth_path.to_string_lossy(),
        "credential_status":"unknown",
        "enabled":metadata.get("enabled").cloned().unwrap_or(Value::Bool(true)),
        "hidden_models":metadata.get("hidden_models").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
        "model_context_windows":metadata.get("model_context_windows").cloned().unwrap_or_else(|| Value::Object(serde_json::Map::new())),
    });
    let account = normalize_account(&raw).map_err(|error| error.to_string())?;
    let prefix = account
        .get("prefix")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let existing = current
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if existing.iter().any(|item| {
        item.get("id").and_then(Value::as_str) != Some(account_id)
            && item.get("prefix").and_then(Value::as_str) == Some(prefix)
    }) {
        return Err(format!("account prefix is already in use: {prefix}"));
    }
    let mut accounts = existing
        .into_iter()
        .filter(|item| item.get("id").and_then(Value::as_str) != Some(account_id))
        .collect::<Vec<_>>();
    accounts.push(account.clone());
    let mut updated = current.clone();
    updated["accounts"] = Value::Array(accounts);
    let mut updated = normalize_configuration(Some(&updated)).map_err(|error| error.to_string())?;
    let config_toml = auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.toml");
    let mut transaction = FileTransaction::new();
    let operation = (|| -> Result<Value, String> {
        transaction
            .remember(&auth_path)
            .map_err(|error| error.to_string())?;
        transaction
            .remember(&config_toml)
            .map_err(|error| error.to_string())?;
        state
            .backend
            .vault
            .write_encrypted_json(&auth_path, &auth)
            .map_err(|error| error.to_string())?;
        std::fs::write(&config_toml, b"cli_auth_credentials_store = \"file\"\n")
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&config_toml, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| error.to_string())?;
        }
        let duplicates = catalog_api::duplicate_accounts(
            &updated,
            &state.backend.vault,
            &state.backend.native_auth_path,
        );
        (updated, _) = emp_state::migrate_duplicate_native_visibility(&updated, &duplicates);
        save_configuration_in_transaction(
            &updated,
            Some(&state.backend.config_path),
            &state.backend.vault,
            &mut transaction,
        )
        .map_err(|error| error.to_string())?;
        load_configuration(Some(&state.backend.config_path)).map_err(|error| error.to_string())
    })();
    let committed = match operation {
        Ok(config) => {
            transaction.commit();
            config
        }
        Err(error) => {
            let _ = transaction.rollback();
            return Err(error);
        }
    };
    *state
        .backend
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())? = committed;
    notify_quota_update(state, account_id, None);
    account_public_snapshot(state, account_id).ok_or_else(|| "account import failed".to_owned())
}

fn delete_account_state(state: &ServerState, account_id: &str) -> Result<(), String> {
    let current = state
        .backend
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())?
        .clone();
    let accounts = current
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target = accounts
        .iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
        .ok_or_else(|| format!("unknown account: {account_id}"))?;
    let expected = emp_state::account_auth_path(&current, account_id, &state.backend.config_path)
        .map_err(|error| error.to_string())?;
    let configured = target
        .get("auth_file")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| "refusing to delete credentials outside the account store".to_owned())?;
    if configured != expected {
        return Err("refusing to delete credentials outside the account store".to_owned());
    }
    let owner = quota_owner_key(state, account_id).map_err(|error| error.to_string())?;
    let mut updated = current.clone();
    updated["accounts"] = Value::Array(
        accounts
            .into_iter()
            .filter(|account| account.get("id").and_then(Value::as_str) != Some(account_id))
            .collect(),
    );
    if updated
        .get("subscription_search")
        .and_then(Value::as_object)
        .and_then(|search| search.get("account_id"))
        .and_then(Value::as_str)
        == Some(account_id)
        && let Some(search) = updated
            .get_mut("subscription_search")
            .and_then(Value::as_object_mut)
    {
        search.insert("enabled".to_owned(), Value::Bool(false));
        search.insert("account_id".to_owned(), Value::String(String::new()));
    }
    let config_toml = expected
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("config.toml");
    let mut transaction = FileTransaction::new();
    let operation = (|| -> Result<Value, String> {
        transaction
            .remember(&expected)
            .map_err(|error| error.to_string())?;
        transaction
            .remember(&config_toml)
            .map_err(|error| error.to_string())?;
        save_configuration_in_transaction(
            &updated,
            Some(&state.backend.config_path),
            &state.backend.vault,
            &mut transaction,
        )
        .map_err(|error| error.to_string())?;
        for path in [&expected, &config_toml] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        load_configuration(Some(&state.backend.config_path)).map_err(|error| error.to_string())
    })();
    let committed = match operation {
        Ok(config) => {
            transaction.commit();
            config
        }
        Err(error) => {
            let _ = transaction.rollback();
            return Err(error);
        }
    };
    *state
        .backend
        .config
        .lock()
        .map_err(|_| "internal server error".to_owned())? = committed;
    if owner == account_id {
        state
            .backend
            .quota_history
            .delete_account(account_id)
            .map_err(|error| error.to_string())?;
    }
    notify_quota_update(state, account_id, None);
    Ok(())
}

fn management_account_import_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    match import_account_state(state, &body) {
        Ok(account) => {
            let body = serde_json::to_vec(&serde_json::json!({"account":account})).unwrap();
            response("HTTP/1.1 200 OK", "application/json", &body, &[])
        }
        Err(error) => json_error_response(400, status_text(400), &error, None, &[]),
    }
}

fn route_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    route_request_at(request, state, system_now())
}

fn route_request_at(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let path = request.raw_path();
    if request.method == RequestMethod::Get
        && (path == "/v1/models" || path.starts_with("/v1/models/"))
    {
        return catalog_api::models_request(request, state);
    }
    if path == "/healthz" {
        return health_response();
    }

    let same_origin = same_origin(request, state.port);
    if path == "/" || path == "/index.html" {
        if !same_origin {
            return cross_origin_response("cross-origin Web UI request rejected");
        }
        let supplied_cookie = request.session_cookie();
        if state.sessions.contains(supplied_cookie.as_deref(), now) {
            let Some(cookie) = state.sessions.refresh_header(now) else {
                return login_response();
            };
            return ui_response(&cookie);
        }
        let Some(supplied) = bootstrap_value(request.target) else {
            return login_response();
        };
        if !state.bootstrap.matches(&supplied) {
            return login_response();
        }
        let Some(cookie) = state.sessions.refresh_or_rotate_header(now) else {
            return login_response();
        };
        if !state.bootstrap.consume() {
            return login_response();
        }
        return redirect_response(&cookie);
    }

    if path.starts_with("/api/") {
        if !same_origin {
            return cross_origin_response("management session is required");
        }
        let supplied_cookie = request.session_cookie();
        if state.sessions.contains(supplied_cookie.as_deref(), now) {
            if request.method == RequestMethod::Get
                && (path == "/api/config"
                    || (path.starts_with("/api/accounts/") && path.ends_with("/models")))
            {
                return catalog_api::read_management_request(request, state);
            }
            if request.method == RequestMethod::Get && path == "/api/accounts" {
                let Some(snapshot) = accounts_snapshot(state) else {
                    return json_error_response(
                        500,
                        status_text(500),
                        "internal server error",
                        None,
                        &[],
                    );
                };
                let body =
                    serde_json::to_vec(&snapshot).expect("account snapshot is JSON serializable");
                return response("HTTP/1.1 200 OK", "application/json", &body, &[]);
            }
            if request.method == RequestMethod::Get
                && path.starts_with("/api/accounts/")
                && path.ends_with("/quota-history")
            {
                let raw_account =
                    &path["/api/accounts/".len()..path.len() - "/quota-history".len()];
                let account_id = percent_decode(raw_account, false);
                let range = query_values(request.target, "range")
                    .into_iter()
                    .find(|value| !value.is_empty())
                    .unwrap_or_else(|| "1d".to_owned());
                return match quota_history_response(state, &account_id, &range, now.trunc() as i64)
                {
                    Ok(payload) => {
                        let body = serde_json::to_vec(&payload)
                            .expect("quota history snapshot is serializable");
                        response("HTTP/1.1 200 OK", "application/json", &body, &[])
                    }
                    Err(QuotaHistoryResponseError::History(error)) => {
                        json_error_response(400, status_text(400), &error.to_string(), None, &[])
                    }
                    Err(QuotaHistoryResponseError::Account(error)) => {
                        json_error_response(404, status_text(404), &error.to_string(), None, &[])
                    }
                };
            }
            if request.method == RequestMethod::Delete && path.starts_with("/api/accounts/") {
                let account_id =
                    percent_decode(path["/api/accounts/".len()..].trim_end_matches('/'), false);
                return match delete_account_state(state, &account_id) {
                    Ok(()) => {
                        let body = serde_json::to_vec(&serde_json::json!({"status":"ok"})).unwrap();
                        response("HTTP/1.1 200 OK", "application/json", &body, &[])
                    }
                    Err(error) => json_error_response(400, status_text(400), &error, None, &[]),
                };
            }
            return not_found_response();
        }
        return unauthorized_response();
    }

    not_found_response()
}

pub struct ServerHandle {
    local_addr: SocketAddr,
    state: Arc<ServerState>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

struct ServerState {
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
    sessions: Arc<SessionStore>,
    bootstrap: BootstrapToken,
    backend: BackendState,
    port: u16,
}

struct BackendState {
    config: Mutex<Value>,
    discovery_lock: Mutex<()>,
    config_path: PathBuf,
    vault: VaultStore,
    client: HttpClient,
    runtime: Runtime,
    request_limits: Arc<RequestLimits>,
    native_auth_path: PathBuf,
    codex_home: PathBuf,
    codex_binary: String,
    native_quota: Mutex<Option<Value>>,
    quota_refresh_errors: Mutex<BTreeMap<String, String>>,
    quota_refresh_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    quota_history: QuotaHistoryStore,
    quota_revision: Mutex<u64>,
    quota_condition: Condvar,
    quota_event_slots: AtomicUsize,
    quota_sampler_wait: Mutex<()>,
    quota_sampler_condition: Condvar,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct QuotaSampleCounts {
    sampled: usize,
    failed: usize,
}

fn quota_sample_targets(state: &ServerState) -> Vec<String> {
    let mut targets = Vec::new();
    if regular_file(&state.backend.native_auth_path) {
        targets.push("@native".to_owned());
    }
    let accounts = state
        .backend
        .config
        .lock()
        .ok()
        .and_then(|config| config.get("accounts").and_then(Value::as_array).cloned())
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
        if quota_owner_key(state, account_id).as_deref() == Ok(account_id) {
            targets.push(account_id.to_owned());
        }
    }
    targets
}

fn refresh_account_serialized(state: &ServerState, account_id: &str) -> Result<Value, QuotaError> {
    let refresh_lock = state
        .backend
        .quota_refresh_locks
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?
        .entry(account_id.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _guard = refresh_lock
        .lock()
        .map_err(|_| QuotaError::new("Codex account quota check failed", "quota_error"))?;
    refresh_account_by_id(state, account_id)
}

fn sample_quotas_once(state: &Arc<ServerState>) -> QuotaSampleCounts {
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

impl ServerHandle {
    pub fn start(host: IpAddr, port: u16) -> Result<Self, AppError> {
        Self::start_with_config(host, port, &config_path())
    }

    pub fn start_with_config(
        host: IpAddr,
        port: u16,
        config_path: &Path,
    ) -> Result<Self, AppError> {
        Self::start_with_config_options(host, port, config_path, "codex", codex_auth_path())
    }

    fn start_with_config_options(
        host: IpAddr,
        port: u16,
        config_path: &Path,
        codex_binary: &str,
        native_auth_path: PathBuf,
    ) -> Result<Self, AppError> {
        if !is_loopback(host) {
            return Err(AppError::HostNotLoopback);
        }
        let now = system_now();
        let session_path = web_session_path(config_path)?;
        let session =
            load_or_create_web_session(&session_path, now).map_err(AppError::WebSession)?;
        let mut config = load_configuration(Some(config_path))?;
        let state_root = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state");
        let vault = VaultStore::from_environment(&state_root.join("master.key"))?;
        let duplicates = catalog_api::duplicate_accounts(&config, &vault, &native_auth_path);
        let (migrated, changed) =
            emp_state::migrate_duplicate_native_visibility(&config, &duplicates);
        config = migrated;
        if changed
            || config
                .get("providers")
                .and_then(Value::as_array)
                .is_some_and(|providers| {
                    providers.iter().any(|provider| {
                        provider
                            .get("api_key")
                            .and_then(Value::as_str)
                            .is_some_and(|key| !key.is_empty())
                    })
                })
        {
            save_configuration(&config, Some(config_path), &vault)?;
            config = load_configuration(Some(config_path))?;
        }
        let client = HttpClient::new(HttpClientPolicy::new(
            ProxyPolicy::from_environment(ProxyEnvironment::capture()),
            TimeoutPolicy::default(),
        ))?;
        let runtime = RuntimeBuilder::new_multi_thread()
            .enable_all()
            .thread_name("emp-upstream")
            .build()?;
        let request_limits = RequestLimits::new(
            RequestLimitsConfig::default(),
            || None,
            || system_now().max(0.0) as u64,
            random_hex(8)?,
        )?;
        let codex_home = native_auth_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let backend = BackendState {
            config: Mutex::new(config),
            discovery_lock: Mutex::new(()),
            config_path: config_path.to_path_buf(),
            vault,
            client,
            runtime,
            request_limits,
            native_auth_path,
            codex_home,
            codex_binary: codex_binary.to_owned(),
            native_quota: Mutex::new(None),
            quota_refresh_errors: Mutex::new(BTreeMap::new()),
            quota_refresh_locks: Mutex::new(BTreeMap::new()),
            quota_history: QuotaHistoryStore::new(state_root.join("quota_history.sqlite3")),
            quota_revision: Mutex::new(0),
            quota_condition: Condvar::new(),
            quota_event_slots: AtomicUsize::new(0),
            quota_sampler_wait: Mutex::new(()),
            quota_sampler_condition: Condvar::new(),
        };
        Self::start_with_session(host, port, session_path, session, backend)
    }

    fn start_with_session(
        host: IpAddr,
        port: u16,
        session_path: PathBuf,
        session: WebSession,
        backend: BackendState,
    ) -> Result<Self, AppError> {
        let listener = TcpListener::bind((host, port))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionStore {
            session: Mutex::new(session),
            path: session_path,
        });
        let mut random = [0_u8; WEB_SESSION_TOKEN_BYTES];
        getrandom::getrandom(&mut random).map_err(|_| AppError::RandomUnavailable)?;
        let state = Arc::new(ServerState {
            listener,
            shutdown,
            sessions,
            bootstrap: BootstrapToken {
                token: URL_SAFE_NO_PAD.encode(random),
                used: AtomicBool::new(false),
            },
            backend,
            port: local_addr.port(),
        });
        let workers = Arc::new(Mutex::new(Vec::new()));
        let handle = Self {
            local_addr,
            state,
            workers,
        };
        handle.add_worker()?;
        handle.add_quota_sampler()?;
        Ok(handle)
    }

    fn add_worker(&self) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-http".to_string())
            .spawn(move || {
                loop {
                    if state.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    match state.listener.accept() {
                        Ok((stream, _)) => {
                            let request_state = Arc::clone(&state);
                            let _ = thread::Builder::new()
                                .name("emp-request".to_string())
                                .spawn(move || handle_connection(stream, &request_state));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })
            .map_err(AppError::Io)?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        Ok(())
    }

    fn add_quota_sampler(&self) -> Result<(), AppError> {
        let state = Arc::clone(&self.state);
        let worker = thread::Builder::new()
            .name("emp-quota-sampler".to_owned())
            .spawn(move || {
                while !state.shutdown.load(Ordering::Acquire) {
                    let deadline = Instant::now() + QUOTA_SAMPLE_INTERVAL;
                    let mut wait = match state.backend.quota_sampler_wait.lock() {
                        Ok(wait) => wait,
                        Err(_) => return,
                    };
                    loop {
                        if state.shutdown.load(Ordering::Acquire) {
                            return;
                        }
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let result = state
                            .backend
                            .quota_sampler_condition
                            .wait_timeout(wait, deadline.saturating_duration_since(now));
                        match result {
                            Ok((next, _)) => wait = next,
                            Err(_) => return,
                        }
                    }
                    drop(wait);
                    if !state.shutdown.load(Ordering::Acquire) {
                        sample_quotas_once(&state);
                    }
                }
            })
            .map_err(AppError::Io)?;
        if let Ok(mut workers) = self.workers.lock() {
            workers.push(worker);
        }
        Ok(())
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn bootstrap_url(&self) -> String {
        format!(
            "http://{}/?bootstrap={}",
            self.local_addr, self.state.bootstrap.token
        )
    }

    pub fn session_cookie(&self) -> String {
        let session = self
            .state
            .sessions
            .session
            .lock()
            .expect("session lock is not poisoned");
        session_cookie(session.token(), session.remaining_seconds_at(system_now()))
    }

    pub fn shutdown(self) -> Result<(), AppError> {
        self.state.shutdown.store(true, Ordering::Release);
        self.state.backend.quota_condition.notify_all();
        self.state.backend.quota_sampler_condition.notify_all();
        let workers = match Arc::try_unwrap(self.workers) {
            Ok(workers) => workers,
            Err(_) => return Err(AppError::ServerStopped),
        };
        let workers: Vec<JoinHandle<()>> =
            workers.into_inner().map_err(|_| AppError::ServerStopped)?;
        for worker in workers {
            let _: () = worker.join().map_err(|_| AppError::ServerStopped)?;
        }
        Ok(())
    }
}

pub fn run_server(config: Option<&Path>, host: IpAddr, port: u16) -> Result<(), AppError> {
    let server = match config {
        Some(config) => ServerHandle::start_with_config(host, port, config)?,
        None => ServerHandle::start(host, port)?,
    };
    let local_addr = server.local_addr();
    println!("EMP listening on http://{local_addr}");
    println!("Shutdown: terminate the process (SIGINT/SIGTERM where supported)");
    println!("Open in browser: {}", server.bootstrap_url());
    loop {
        thread::park();
    }
}

fn print_version() {
    println!("EMP {VERSION}");
}

fn run() -> Result<(), String> {
    match parse_cli(std::env::args().skip(1))? {
        Cli::Version => print_version(),
        Cli::Serve { config, host, port } => {
            run_server(config.as_deref(), host, port).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    mod catalog_api_contract;
    mod config_api_contract;
    mod native_api_contract;
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpStream;
    use std::process::Command;
    use std::sync::mpsc;

    use emp_state::WEB_SESSION_TOKEN_LENGTH;
    use serde_json::json;
    use tempfile::TempDir;

    fn canonical_root(directory: &TempDir) -> PathBuf {
        directory
            .path()
            .canonicalize()
            .expect("canonical temporary root")
    }

    fn complete_response(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("response timeout");
        let mut response = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).expect("read response");
            assert!(count > 0, "response ended before Content-Length bytes");
            response.extend_from_slice(&buffer[..count]);
            let Some(separator) = response.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = std::str::from_utf8(&response[..separator]).expect("ASCII headers");
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .expect("Content-Length header");
            let expected = separator + 4 + content_length;
            assert!(response.len() <= expected, "unexpected pipelined bytes");
            if response.len() == expected {
                return String::from_utf8(response).expect("UTF-8 response");
            }
        }
    }

    fn response_until_close(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("response timeout");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        String::from_utf8(response).expect("UTF-8 response")
    }

    fn request(server: &ServerHandle, target: &str, headers: &[&str]) -> String {
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        let host = if headers.iter().any(|header| {
            header
                .split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("host"))
        }) {
            String::new()
        } else {
            format!("Host: 127.0.0.1:{}\r\n", server.local_addr().port())
        };
        let full_headers = headers.join("\r\n");
        stream
            .write_all(
                format!(
                    "GET {target} HTTP/1.1\r\n{host}{full_headers}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("write request");
        complete_response(&mut stream)
    }

    fn post(server: &ServerHandle, target: &str, body: &[u8], headers: &[&str]) -> String {
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        let host = if headers.iter().any(|header| {
            header
                .split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("host"))
        }) {
            String::new()
        } else {
            format!("Host: 127.0.0.1:{}\r\n", server.local_addr().port())
        };
        let content_type = if headers.iter().any(|header| {
            header
                .split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        }) {
            String::new()
        } else {
            "Content-Type: application/json\r\n".to_owned()
        };
        let full_headers = headers.join("\r\n");
        stream
            .write_all(
                format!(
                    "POST {target} HTTP/1.1\r\n{host}{content_type}Content-Length: {}\r\n{full_headers}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .expect("write request head");
        stream.write_all(body).expect("write request body");
        complete_response(&mut stream)
    }

    fn delete(server: &ServerHandle, target: &str, headers: &[&str]) -> String {
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        let full_headers = headers.join("\r\n");
        stream
            .write_all(
                format!(
                    "DELETE {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{full_headers}\r\nConnection: close\r\n\r\n",
                    server.local_addr().port()
                )
                .as_bytes(),
            )
            .expect("write DELETE request");
        complete_response(&mut stream)
    }

    fn read_sse_frame(reader: &mut BufReader<TcpStream>) -> String {
        let mut frame = String::new();
        loop {
            let mut line = String::new();
            let count = reader.read_line(&mut line).expect("read SSE frame");
            assert!(count > 0, "SSE stream ended before a complete frame");
            if line == "\r\n" || line == "\n" {
                return frame;
            }
            frame.push_str(&line);
        }
    }

    fn open_quota_events(server: &ServerHandle, cookie: &str) -> BufReader<TcpStream> {
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect SSE");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("SSE response timeout");
        stream
            .write_all(
                format!(
                    "GET /api/accounts/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{cookie}\r\nConnection: close\r\n\r\n",
                    server.local_addr().port()
                )
                .as_bytes(),
            )
            .expect("write SSE request");
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read SSE headers");
            assert!(!line.is_empty(), "SSE response ended before headers");
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: text/event-stream\r\n"));
        assert!(head.contains("Cache-Control: no-store\r\n"));
        assert!(head.contains("X-Accel-Buffering: no\r\n"));
        assert_eq!(
            read_sse_frame(&mut reader),
            "event: quota-updated\ndata: {}\n"
        );
        reader
    }

    fn open_post_stream(
        server: &ServerHandle,
        target: &str,
        body: &[u8],
        headers: &[&str],
    ) -> TcpStream {
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        let full_headers = headers.join("\r\n");
        stream
            .write_all(
                format!(
                    "POST {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{full_headers}\r\nConnection: close\r\n\r\n",
                    server.local_addr().port(),
                    body.len()
                )
                .as_bytes(),
            )
            .expect("write stream request head");
        stream.write_all(body).expect("write stream request body");
        stream
    }

    fn post_stream(server: &ServerHandle, target: &str, body: &[u8], headers: &[&str]) -> String {
        let mut stream = open_post_stream(server, target, body, headers);
        response_until_close(&mut stream)
    }

    struct OneShotUpstream {
        address: SocketAddr,
        observed: mpsc::Receiver<(String, BTreeMap<String, String>, Value)>,
        worker: Option<JoinHandle<()>>,
    }

    fn receive_upstream_request(
        stream: &mut TcpStream,
    ) -> (String, BTreeMap<String, String>, Value) {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("upstream timeout");
        let raw = read_request_head(stream).expect("upstream request head");
        let request = parse_request(&raw.head).expect("upstream HTTP request");
        let path = request.target.to_owned();
        let headers = request
            .headers
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<BTreeMap<_, _>>();
        let length = headers
            .get("content-length")
            .map(|value| value.parse::<usize>().expect("upstream Content-Length"))
            .unwrap_or(0);
        let mut body = raw.body_prefix;
        while body.len() < length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).expect("read upstream body");
            assert!(count > 0, "upstream body ended early");
            body.extend_from_slice(&chunk[..count]);
        }
        body.truncate(length);
        let body = if body.is_empty() {
            Value::Null
        } else {
            let decoded = emp_transport::decode_content(
                body,
                headers
                    .get("content-encoding")
                    .map(String::as_str)
                    .unwrap_or(""),
                4 * 1024 * 1024,
                None,
            )
            .expect("decode upstream request");
            serde_json::from_slice(&decoded).expect("upstream request JSON")
        };
        (path, headers, body)
    }

    impl OneShotUpstream {
        fn start(response_body: Value) -> Self {
            let encoded = serde_json::to_vec(&response_body).expect("upstream response JSON");
            Self::start_wire(200, "application/json", None, vec![encoded])
        }

        fn start_sse(chunks: Vec<Vec<u8>>) -> Self {
            Self::start_wire(200, "text/event-stream", None, chunks)
        }

        fn start_error(status: u16, retry_after: Option<u64>, response_body: Value) -> Self {
            let encoded = serde_json::to_vec(&response_body).expect("upstream error JSON");
            Self::start_wire(status, "application/json", retry_after, vec![encoded])
        }

        fn start_wire(
            status: u16,
            content_type: &'static str,
            retry_after: Option<u64>,
            chunks: Vec<Vec<u8>>,
        ) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
            let address = listener.local_addr().expect("upstream address");
            let (sender, observed) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept upstream");
                let (path, headers, body) = receive_upstream_request(&mut stream);
                sender
                    .send((path, headers, body))
                    .expect("record upstream request");
                let content_length = chunks.iter().map(Vec::len).sum::<usize>();
                let retry_after = retry_after
                    .map(|delay| format!("Retry-After: {delay}\r\n"))
                    .unwrap_or_default();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\n{retry_after}Connection: close\r\n\r\n",
                            status_text(status)
                        )
                        .as_bytes(),
                    )
                    .expect("write upstream response head");
                for chunk in chunks {
                    stream
                        .write_all(&chunk)
                        .expect("write upstream response body");
                    stream.flush().expect("flush upstream response body");
                }
            });
            Self {
                address,
                observed,
                worker: Some(worker),
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}/v1", self.address)
        }

        fn observed(&self) -> (String, BTreeMap<String, String>, Value) {
            self.observed
                .recv_timeout(Duration::from_secs(5))
                .expect("upstream observation")
        }
    }

    impl Drop for OneShotUpstream {
        fn drop(&mut self) {
            if let Some(worker) = self.worker.take() {
                if !worker.is_finished()
                    && let Ok(mut stream) = TcpStream::connect(self.address)
                {
                    let _ = stream.write_all(
                        b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
                    );
                }
                worker.join().expect("join upstream");
            }
        }
    }

    fn fallback_upstream(
        content_type: &'static str,
        success_body: Vec<u8>,
    ) -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
        two_attempt_upstream(404, None, content_type, success_body)
    }

    fn two_attempt_upstream(
        first_status: u16,
        retry_after: Option<u64>,
        content_type: &'static str,
        success_body: Vec<u8>,
    ) -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fallback upstream");
        let address = listener.local_addr().expect("fallback upstream address");
        let (path_sender, paths) = mpsc::sync_channel(2);
        let worker = thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept fallback upstream");
                let (path, _, _) = receive_upstream_request(&mut stream);
                path_sender.send(path).expect("record fallback path");
                let (status, response_type, body) = if attempt == 0 {
                    (
                        first_status,
                        "application/json",
                        br#"{"error":{"message":"temporary upstream rejection"}}"#.to_vec(),
                    )
                } else {
                    (200, content_type, success_body.clone())
                };
                let retry_header = (attempt == 0)
                    .then_some(retry_after)
                    .flatten()
                    .map(|delay| format!("Retry-After: {delay}\r\n"))
                    .unwrap_or_default();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} {}\r\nContent-Type: {response_type}\r\nContent-Length: {}\r\n{retry_header}Connection: close\r\n\r\n",
                            status_text(status),
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .expect("write fallback response head");
                stream.write_all(&body).expect("write fallback body");
            }
        });
        (format!("http://{address}/v1"), paths, worker)
    }

    fn configured_server(base_url: &str) -> (TempDir, ServerHandle) {
        configured_protocol_server(base_url, "chat_completions", "api_key")
    }

    fn configured_protocol_server(
        base_url: &str,
        protocol: &str,
        auth_mode: &str,
    ) -> (TempDir, ServerHandle) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = canonical_root(&directory).join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "providers": [{
                    "id": "demo", "name": "Demo", "base_url": base_url,
                    "protocol": protocol, "auth_mode": auth_mode,
                    "api_key": "upstream-secret"
                }],
                "models": [{
                    "id": "demo/model", "provider": "demo",
                    "upstream_id": "upstream-model", "enabled": true
                }]
            }))
            .expect("encode config"),
        )
        .expect("write config");
        let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
            .expect("start configured server");
        (directory, server)
    }

    #[test]
    fn native_catalog_model_reaches_the_native_transport_boundary() {
        let upstream = OneShotUpstream::start(json!({
            "id":"resp_native_fixture","object":"response","status":"completed",
            "model":"gpt-native-fixture","output":[]
        }));
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = canonical_root(&directory);
        let catalog = root.join("models_cache.json");
        std::fs::write(
            &catalog,
            serde_json::to_vec(&json!({
                "models": [{
                    "slug": "gpt-native-fixture",
                    "context_window": 272000,
                    "supported_in_api": true
                }]
            }))
            .expect("encode native catalog"),
        )
        .expect("write native catalog");
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "native_catalog_path": catalog,
                "codex_base_url": upstream.base_url(),
                "providers":[{
                    "id":"native-forward","base_url":upstream.base_url(),
                    "protocol":"responses","auth_mode":"forward"
                }]
            }))
            .expect("encode config"),
        )
        .expect("write config");
        let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
            .expect("start native catalog server");
        let body = serde_json::to_vec(&json!({
            "model": "gpt-native-fixture",
            "input": "hello",
            "stream": false
        }))
        .expect("request JSON");
        let response = post(
            &server,
            "/v1/responses",
            &body,
            &[
                &session_cookie_header(&server),
                "Authorization: Bearer native-fixture",
            ],
        );
        assert!(
            response.starts_with("HTTP/1.1 200 OK\r\n"),
            "catalog model must cross the native transport boundary: {response}"
        );
        let (path, headers, body) = upstream.observed();
        assert_eq!(path, "/v1/responses");
        assert_eq!(headers["content-encoding"], "zstd");
        assert_eq!(headers["authorization"], "Bearer native-fixture");
        assert_eq!(body["model"], "gpt-native-fixture");
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn quota_events_are_bounded_authenticated_and_revision_driven() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = canonical_root(&directory).join("config.json");
        std::fs::write(&config, b"{}").expect("write config");
        let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
            .expect("start quota event server");
        let cookie = session_cookie_header(&server);

        let denied = request(&server, "/api/accounts/events", &[]);
        assert!(
            denied.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
            "{denied}"
        );
        let cross_origin = request(
            &server,
            "/api/accounts/events",
            &[&cookie, "Origin: https://example.invalid"],
        );
        assert!(
            cross_origin.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{cross_origin}"
        );

        let mut streams = (0..QUOTA_EVENT_SLOT_LIMIT)
            .map(|_| open_quota_events(&server, &cookie))
            .collect::<Vec<_>>();
        let excess = request(&server, "/api/accounts/events", &[&cookie]);
        assert!(
            excess.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{excess}"
        );
        assert!(excess.contains("Retry-After: 15\r\n"));

        notify_quota_update(&server.state, "@native", Some("quota_auth_required"));
        assert_eq!(
            read_sse_frame(&mut streams[0]),
            "event: quota-updated\ndata: {}\n"
        );
        let accounts = request(&server, "/api/accounts", &[&cookie]);
        assert!(accounts.starts_with("HTTP/1.1 200 OK\r\n"), "{accounts}");
        let accounts: Value = serde_json::from_str(
            accounts
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("account state");
        assert_eq!(accounts["refresh_errors"]["@native"], "quota_auth_required");

        notify_quota_update(&server.state, "@native", None);
        assert_eq!(
            read_sse_frame(&mut streams[0]),
            "event: quota-updated\ndata: {}\n"
        );
        server.shutdown().expect("shutdown");
        let mut tail = Vec::new();
        streams[0]
            .read_to_end(&mut tail)
            .expect("quota stream closes during shutdown");
    }

    #[cfg(unix)]
    #[test]
    fn native_and_imported_quota_refresh_cross_the_management_boundary() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory");
        let root = canonical_root(&directory);
        let auth_path = root.join("auth.json");
        let original_auth = serde_json::to_vec(&json!({
            "tokens": {"access_token": "native-secret", "account_id": "workspace"}
        }))
        .expect("encode auth");
        std::fs::write(&auth_path, &original_auth).expect("write native auth");
        let account_root = root.join("state").join("accounts");
        let imported_auth = account_root.join("egg").join("auth.json.enc");
        let duplicate_auth = account_root.join("native-copy").join("auth.json.enc");
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "native_hidden_models": ["gpt-hidden"],
                "native_model_context_windows": {"gpt-visible": 200000},
                "account_store_path": account_root,
                "accounts": [{
                    "id": "egg",
                    "name": "egg",
                    "prefix": "egg",
                    "auth_file": imported_auth,
                }, {
                    "id": "native-copy",
                    "name": "Native copy",
                    "prefix": "native-copy",
                    "auth_file": duplicate_auth,
                }]
            }))
            .expect("encode config"),
        )
        .expect("write config");

        let target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("target");
        std::fs::create_dir_all(&target).expect("create target directory");
        let executable_root = tempfile::Builder::new()
            .prefix("emp-fake-codex-")
            .tempdir_in(target)
            .expect("fake Codex directory");
        let executable = executable_root.path().join("codex");
        std::fs::write(
            &executable,
            r#"#!/usr/bin/env python3
import json, pathlib, os, sys
home = pathlib.Path(os.environ["CODEX_HOME"])
started = ""
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id": request["id"], "result": {}}), flush=True)
    elif method == "account/read":
        auth = json.loads((home / "auth.json").read_text())
        started = auth["tokens"]["access_token"]
        if started == "imported-original":
            assert request["params"] == {"refreshToken": False}
            auth["tokens"]["access_token"] = "imported-rotated"
            (home / "auth.json").write_text(json.dumps(auth))
        elif started == "imported-rotated":
            assert request["params"] in ({"refreshToken": False}, {"refreshToken": True})
        print(json.dumps({"id": request["id"], "result": {"account": {"email": "xian@example.com", "planType": "pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        if started == "imported-original":
            print(json.dumps({"id": request["id"], "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 401 Unauthorized; content-type=text/plain; body=private-token"}}), flush=True)
        else:
            used = 11 if started == "imported-rotated" else 7
            if started == "native-secret":
                auth = json.loads((home / "auth.json").read_text())
                auth["tokens"]["access_token"] = "isolated-rotation"
                (home / "auth.json").write_text(json.dumps(auth))
            print(json.dumps({"id": request["id"], "result": {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": used, "windowDurationMins": 300}}}}), flush=True)
    elif method == "account/rateLimitResetCredit/consume":
        assert request["params"] == {"idempotencyKey": "12345678-1234-4123-8123-123456789abc"}
        print(json.dumps({"id": request["id"], "result": {"outcome": "reset"}}), flush=True)
"#,
        )
        .expect("write fake Codex");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("make fake Codex executable");

        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            executable.to_str().expect("UTF-8 executable path"),
            auth_path.clone(),
        )
        .expect("start quota server");
        server
            .state
            .backend
            .vault
            .write_encrypted_json(
                &imported_auth,
                &json!({
                    "tokens": {
                        "access_token": "imported-original",
                        "account_id": "workspace-egg"
                    }
                }),
            )
            .expect("write imported auth");
        server
            .state
            .backend
            .vault
            .write_encrypted_json(
                &duplicate_auth,
                &json!({
                    "tokens": {
                        "access_token": "stale-native-snapshot",
                        "account_id": "workspace"
                    }
                }),
            )
            .expect("write duplicate auth");
        let cookie = session_cookie_header(&server);
        let before = request(&server, "/api/accounts", &[&cookie]);
        assert!(before.starts_with("HTTP/1.1 200 OK\r\n"), "{before}");
        let before: Value =
            serde_json::from_str(before.split_once("\r\n\r\n").expect("response separator").1)
                .expect("account snapshot");
        assert_eq!(before["native_account"]["credential_set"], true);
        assert_eq!(before["native_account"]["quota"], Value::Null);

        let refreshed = post(&server, "/api/accounts/%40native/quota", b"{}", &[&cookie]);
        assert!(refreshed.starts_with("HTTP/1.1 200 OK\r\n"), "{refreshed}");
        let refreshed: Value = serde_json::from_str(
            refreshed
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("refreshed account");
        assert_eq!(refreshed["account"]["quota"]["plan_type"], "pro");
        assert_eq!(
            refreshed["account"]["quota"]["rate_limits"]["primary"]["usedPercent"],
            7
        );
        assert_eq!(
            std::fs::read(&auth_path).expect("native auth after refresh"),
            original_auth,
            "native quota refresh must never persist isolated token rotation"
        );

        let reset_body = serde_json::to_vec(&json!({
            "idempotency_key": "12345678-1234-4123-8123-123456789ABC"
        }))
        .expect("reset request");
        let reset = post(
            &server,
            "/api/accounts/%40native/quota-reset",
            &reset_body,
            &[&cookie],
        );
        assert!(reset.starts_with("HTTP/1.1 200 OK\r\n"), "{reset}");
        let reset: Value =
            serde_json::from_str(reset.split_once("\r\n\r\n").expect("response separator").1)
                .expect("reset response");
        assert_eq!(reset["outcome"], "reset");
        assert_eq!(reset["account"]["quota"]["plan_type"], "pro");
        assert_eq!(reset["refresh_error"], Value::Null);

        let invalid_reset = post(
            &server,
            "/api/accounts/%40native/quota-reset",
            br#"{"idempotency_key":"retry-me"}"#,
            &[&cookie],
        );
        assert!(
            invalid_reset.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{invalid_reset}"
        );
        assert!(invalid_reset.contains("quota_reset_invalid_request"));

        let imported = post(&server, "/api/accounts/egg/quota", b"{}", &[&cookie]);
        assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
        let imported: Value = serde_json::from_str(
            imported
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("imported account response");
        assert_eq!(imported["account"]["credential_status"], "valid");
        assert_eq!(
            imported["account"]["quota"]["rate_limits"]["primary"]["usedPercent"],
            11
        );
        let persisted_auth = server
            .state
            .backend
            .vault
            .read_encrypted_json(&imported_auth)
            .expect("read rotated imported auth");
        assert_eq!(
            persisted_auth["tokens"]["access_token"], "imported-rotated",
            "a rotation completed before the first 401 must be reused by the retry"
        );

        let duplicate = post(
            &server,
            "/api/accounts/native-copy/quota",
            b"{}",
            &[&cookie],
        );
        assert!(duplicate.starts_with("HTTP/1.1 200 OK\r\n"), "{duplicate}");
        let duplicate: Value = serde_json::from_str(
            duplicate
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("duplicate account response");
        assert_eq!(
            duplicate["account"]["quota"]["rate_limits"]["primary"]["usedPercent"], 7,
            "a duplicate account must query the live native credential"
        );
        assert_eq!(
            server
                .state
                .backend
                .vault
                .read_encrypted_json(&duplicate_auth)
                .expect("read duplicate snapshot")["tokens"]["access_token"],
            "stale-native-snapshot",
            "native refresh must not overwrite the imported snapshot"
        );

        let native_history = request(
            &server,
            "/api/accounts/%40native/quota-history?range=all",
            &[&cookie],
        );
        assert!(
            native_history.starts_with("HTTP/1.1 200 OK\r\n"),
            "{native_history}"
        );
        let native_history: Value = serde_json::from_str(
            native_history
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("native quota history");
        assert_eq!(native_history["account_id"], "@native");
        assert_eq!(
            native_history["series"][0]["points"][0]["remaining_percent"],
            93.0
        );
        assert_eq!(native_history["plans"][0]["plan_type"], "pro");

        let duplicate_history = request(
            &server,
            "/api/accounts/native-copy/quota-history?range=all",
            &[&cookie],
        );
        let duplicate_history: Value = serde_json::from_str(
            duplicate_history
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("duplicate quota history");
        assert_eq!(duplicate_history["account_id"], "native-copy");
        assert_eq!(duplicate_history["series"], native_history["series"]);

        let imported_history = request(
            &server,
            "/api/accounts/egg/quota-history?range=all",
            &[&cookie],
        );
        let imported_history: Value = serde_json::from_str(
            imported_history
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("imported quota history");
        assert_eq!(
            imported_history["series"][0]["points"][0]["remaining_percent"],
            89.0
        );

        let invalid_history = request(
            &server,
            "/api/accounts/egg/quota-history?range=forever",
            &[&cookie],
        );
        assert!(
            invalid_history.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{invalid_history}"
        );
        let missing_history = request(
            &server,
            "/api/accounts/missing/quota-history?range=all",
            &[&cookie],
        );
        assert!(
            missing_history.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{missing_history}"
        );
        assert_eq!(
            sample_quotas_once(&server.state),
            QuotaSampleCounts {
                sampled: 2,
                failed: 0,
            },
            "the sampler must refresh native and the unique imported account while skipping the duplicate"
        );
        server.shutdown().expect("shutdown");
    }

    fn assert_saved_protocol_observation(
        directory: &TempDir,
        server: &ServerHandle,
        expected: &str,
    ) {
        let saved = load_configuration(Some(&canonical_root(directory).join("config.json")))
            .expect("reload observed config");
        assert_eq!(saved["providers"][0]["resolved_protocol"], expected);
        assert_eq!(saved["models"][0]["resolved_protocol"], expected);
        assert_eq!(
            saved["providers"][0]["protocol_observation"],
            saved["models"][0]["protocol_observation"]
        );
        assert_eq!(
            saved["providers"][0]["protocol_observation"]["upstream_model"],
            "upstream-model"
        );
        let config = server.state.backend.config.lock().expect("config lock");
        assert_eq!(config["providers"][0]["resolved_protocol"], expected);
        assert_eq!(
            provider_api_key(&config["providers"][0], &server.state.backend.vault),
            "upstream-secret"
        );
    }

    fn session_cookie_header(server: &ServerHandle) -> String {
        let cookie = server.session_cookie();
        format!(
            "Cookie: {}",
            cookie.split(';').next().expect("session cookie pair")
        )
    }

    fn test_server() -> (TempDir, ServerHandle) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = canonical_root(&directory).join("config.json");
        let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
            .expect("start server");
        (directory, server)
    }

    #[test]
    fn cli_accepts_optional_config() {
        let parsed = parse_cli([
            "serve".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            "0".to_string(),
        ])
        .expect("parse CLI");
        assert_eq!(
            parsed,
            Cli::Serve {
                config: None,
                host: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 0,
            }
        );
    }

    #[test]
    fn health_stays_unauthenticated() {
        let (_directory, server) = test_server();
        let response = request(&server, "/healthz", &[]);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.ends_with("{\"status\":\"ok\"}"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn login_page_has_the_chinese_contract() {
        let (_directory, server) = test_server();
        let response = request(&server, "/", &[]);
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(response.contains("请从 EMP 打开管理页"));
        assert!(!response.contains("Set-Cookie"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn malformed_and_duplicate_bootstrap_never_login() {
        let (_directory, server) = test_server();
        let long_token = "A".repeat(WEB_SESSION_TOKEN_LENGTH);
        for target in [
            "/?bootstrap=",
            "/?bootstrap=wrong",
            &format!("/?bootstrap={long_token}"),
            &format!(
                "/?bootstrap={}&bootstrap={}",
                server.state.bootstrap.token, server.state.bootstrap.token
            ),
        ] {
            let response = request(&server, target, &[]);
            assert!(
                response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
                "target: {target}"
            );
        }
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn encoded_query_key_and_python_origin_forms_match() {
        let (_directory, server) = test_server();
        let encoded_key = request(
            &server,
            &format!("/?%62ootstrap={}", server.state.bootstrap.token),
            &[],
        );
        assert!(encoded_key.starts_with("HTTP/1.1 303 See Other\r\n"));
        server.shutdown().expect("shutdown");

        let (_directory, server) = test_server();
        let port = server.local_addr().port();
        for origin in [
            format!("Origin: HTTP://LOCALHOST:{port}"),
            format!("Origin: http://localhost:{port:05}"),
            format!("Origin: https://127.0.0.1:{port}/path"),
        ] {
            let response = request(&server, "/", &[&origin]);
            assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        }
        for origin in [
            format!("Origin: http://localhost.:{port}"),
            format!("Origin: http://%31%32%37.0.0.1:{port}"),
        ] {
            let response = request(&server, "/", &[&origin]);
            assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        }
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn percent_encoded_bootstrap_is_decoded_once() {
        let (_directory, server) = test_server();
        let encoded: String = server
            .state
            .bootstrap
            .token
            .chars()
            .map(|character| format!("%{:02X}", character as u8))
            .collect();
        let first = request(&server, &format!("/?bootstrap={encoded}"), &[]);
        assert!(first.starts_with("HTTP/1.1 303 See Other\r\n"));
        assert!(first.contains("Location: /\r\n"));
        let second = request(
            &server,
            &format!("/?bootstrap={}", server.state.bootstrap.token),
            &[],
        );
        assert!(second.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn bootstrap_login_sets_exact_cookie_and_session_serves_ui() {
        let (_directory, server) = test_server();
        let bootstrap = request(
            &server,
            &format!("/?bootstrap={}", server.state.bootstrap.token),
            &[],
        );
        let separator = bootstrap.find("\r\n\r\n").expect("separator");
        let headers = &bootstrap[..separator];
        assert!(headers.contains("\r\nSet-Cookie: emp_session="));
        assert!(headers.contains("; HttpOnly; SameSite=Strict; Path=/; Max-Age="));
        let cookie = headers
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .expect("cookie header");
        let value = cookie.split(';').next().expect("cookie value");
        let session = request(&server, "/", &[&format!("Cookie: {value}")]);
        let body_start = session.find("\r\n\r\n").expect("separator") + 4;
        assert!(session.starts_with("HTTP/1.1 200 OK\r\n"));
        assert_eq!(&session.as_bytes()[body_start..], WEB_INDEX_BYTES);
        let refreshed = session
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .expect("refreshed session cookie");
        assert!(refreshed.starts_with(&format!(
            "{value}; HttpOnly; SameSite=Strict; Path=/; Max-Age="
        )));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn cross_origin_ui_and_api_are_rejected() {
        let (_directory, server) = test_server();
        let origin = format!(
            "Origin: http://127.0.0.1:{}",
            server.local_addr().port() + 1
        );
        let ui = request(&server, "/", &[&origin]);
        assert!(ui.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(ui.contains("cross-origin Web UI request rejected"));
        let api = request(&server, "/api/config", &[&origin]);
        assert!(api.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn api_session_boundary_is_exact() {
        let (_directory, server) = test_server();
        let unauthorized = request(&server, "/api/config", &[]);
        assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        let login = request(
            &server,
            &format!("/?bootstrap={}", server.state.bootstrap.token),
            &[],
        );
        let cookie = login
            .lines()
            .find_map(|line| line.strip_prefix("Set-Cookie: "))
            .expect("cookie header");
        let value = cookie.split(';').next().expect("cookie value");
        let api = request(&server, "/api/config", &[&format!("Cookie: {value}")]);
        assert!(api.starts_with("HTTP/1.1 200 OK\r\n"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn web_session_persists_across_restart() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = canonical_root(&directory).join("config.json");
        let first = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
            .expect("start first server");
        let cookie = first.session_cookie();
        let token = cookie
            .trim_start_matches("emp_session=")
            .split(';')
            .next()
            .expect("token")
            .to_string();
        let first_addr = first.local_addr();
        first.shutdown().expect("shutdown first server");
        let second = ServerHandle::start_with_config(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            first_addr.port(),
            &config,
        )
        .expect("start second server");
        let mut stream = TcpStream::connect(second.local_addr()).expect("connect");
        stream
            .write_all(format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: emp_session={token}\r\nConnection: close\r\n\r\n", second.local_addr().port()).as_bytes())
            .expect("write request");
        assert!(complete_response(&mut stream).starts_with("HTTP/1.1 200 OK\r\n"));
        second.shutdown().expect("shutdown second server");
    }

    #[test]
    fn expired_web_session_is_rotated_at_startup() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let session_path = canonical_root(&directory).join("state/web-session.json");
        std::fs::create_dir_all(session_path.parent().expect("state directory"))
            .expect("create state");
        std::fs::write(
            &session_path,
            br#"{"token":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","expires_at":1}"#,
        )
        .expect("write expired session");
        let server = ServerHandle::start_with_config(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &canonical_root(&directory).join("config.json"),
        )
        .expect("rotate expired session");
        let cookie = server.session_cookie();
        assert!(!cookie.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn bootstrap_rotates_a_session_that_expires_while_running() {
        let (_directory, server) = test_server();
        let (old_token, future) = {
            let session = server.state.sessions.session.lock().expect("session lock");
            (session.token().to_owned(), session.expires_at() + 1.0)
        };
        let raw = format!(
            "GET /?bootstrap={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
            server.state.bootstrap.token,
            server.local_addr().port()
        );
        let request = parse_request(&raw).expect("request");
        let response = route_request_at(request, &server.state, future);
        assert!(response.starts_with(b"HTTP/1.1 303 See Other\r\n"));
        let session = server.state.sessions.session.lock().expect("session lock");
        assert_ne!(session.token(), old_token);
        assert!(session.is_active_at(future));
        drop(session);
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn cookie_parser_uses_the_last_value_and_rejects_malformed_input() {
        assert_eq!(
            parse_session_cookie("emp_session=first; emp_session=second").as_deref(),
            Some("second")
        );
        assert_eq!(
            parse_session_cookie("emp_session=\"quoted\"").as_deref(),
            Some("quoted")
        );
        assert_eq!(parse_session_cookie("emp_session=valid; malformed"), None);
        assert_eq!(parse_session_cookie("emp_session=\"unterminated"), None);
        assert_eq!(
            parse_session_cookie("emp_session=\"é\"").as_deref(),
            Some("é")
        );
    }

    #[test]
    fn caller_authorization_tracks_the_live_native_token() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let auth = directory.path().join("auth.json");
        std::fs::write(&auth, br#"{"tokens":{"access_token":"native-secret"}}"#)
            .expect("write auth");
        assert!(valid_caller_authorization(
            Some("Bearer native-secret"),
            &auth
        ));
        assert!(!valid_caller_authorization(
            Some("bearer native-secret"),
            &auth
        ));
        assert!(!valid_caller_authorization(Some("Bearer wrong"), &auth));
        std::fs::write(&auth, br#"{"access_token":"rotated"}"#).expect("rotate auth");
        assert!(valid_caller_authorization(Some("Bearer rotated"), &auth));
        assert!(!valid_caller_authorization(
            Some("Bearer native-secret"),
            &auth
        ));

        if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            let script = r#"
import json
from easy_multi_provider.accounts import valid_caller_authorization
values = ["Bearer rotated", "bearer rotated", "Bearer wrong", "Bearer ", ""]
print(json.dumps([valid_caller_authorization(value) for value in values]))
"#;
            let output = Command::new(python)
                .arg("-c")
                .arg(script)
                .env("CODEX_HOME", directory.path())
                .current_dir(root)
                .output()
                .expect("spawn Python authorization oracle");
            assert!(
                output.status.success(),
                "Python authorization oracle failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let python: Value =
                serde_json::from_slice(&output.stdout).expect("Python authorization JSON");
            let rust = json!([
                valid_caller_authorization(Some("Bearer rotated"), &auth),
                valid_caller_authorization(Some("bearer rotated"), &auth),
                valid_caller_authorization(Some("Bearer wrong"), &auth),
                valid_caller_authorization(Some("Bearer "), &auth),
                valid_caller_authorization(Some(""), &auth),
            ]);
            assert_eq!(rust, python);
        }
    }

    #[test]
    fn responses_authentication_precedes_request_body_reads() {
        let (_directory, server) = test_server();
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .write_all(
                format!(
                    "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
                    server.local_addr().port()
                )
                .as_bytes(),
            )
            .expect("write unauthenticated head only");
        let response = complete_response(&mut stream);
        assert!(response.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(response.contains("proxy caller authentication is required"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn complete_chat_request_crosses_the_real_server_boundary() {
        let upstream = OneShotUpstream::start(json!({
            "id": "chat_upstream", "model": "upstream-model",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "answer"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }));
        let (_directory, server) = configured_server(&upstream.base_url());
        let request_body = serde_json::to_vec(&json!({
            "model": "demo/model",
            "input": [{
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }],
            "stream": false
        }))
        .expect("request JSON");
        let response = post(
            &server,
            "/v1/responses",
            &request_body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let response_body: Value = serde_json::from_str(
            response
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("response JSON");
        assert_eq!(response_body["model"], "demo/model");
        assert_eq!(response_body["status"], "completed");
        assert_eq!(response_body["output"][0]["content"][0]["text"], "answer");

        let (path, headers, upstream_body) = upstream.observed();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(headers["authorization"], "Bearer upstream-secret");
        assert_eq!(headers["x-emp-request-id"].len(), 16);
        assert_eq!(upstream_body["model"], "upstream-model");
        assert_eq!(upstream_body["stream"], false);
        server.shutdown().expect("shutdown");
    }

    fn chat_summary_upstream(summary: &str) -> OneShotUpstream {
        OneShotUpstream::start(json!({
            "id": "chat_summary", "model": "upstream-model",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": summary},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        }))
    }

    #[test]
    fn external_compact_endpoint_uses_the_selected_model_and_returns_a_portable_checkpoint() {
        let upstream = chat_summary_upstream("portable checkpoint");
        let (_directory, server) = configured_server(&upstream.base_url());
        let request_body = serde_json::to_vec(&json!({
            "model": "demo/model",
            "input": [{
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "history"}]
            }],
            "reasoning":{"effort":"high"}
        }))
        .expect("request JSON");
        let response = post(
            &server,
            "/v1/responses/compact",
            &request_body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let response_body: Value = serde_json::from_str(
            response
                .split_once("\r\n\r\n")
                .expect("response separator")
                .1,
        )
        .expect("response JSON");
        assert_eq!(response_body["model"], "demo/model");
        assert_eq!(response_body["status"], "completed");
        assert_eq!(response_body["usage"], Value::Null);
        let encoded = response_body["output"][0]["encrypted_content"]
            .as_str()
            .expect("checkpoint")
            .strip_prefix("emp1:")
            .expect("portable prefix");
        assert_eq!(
            URL_SAFE.decode(encoded).expect("checkpoint base64"),
            b"portable checkpoint"
        );

        let (path, _, upstream_body) = upstream.observed();
        assert_eq!(path, "/v1/chat/completions");
        assert_eq!(upstream_body["model"], "upstream-model");
        assert_eq!(upstream_body["stream"], false);
        assert!(
            upstream_body["messages"]
                .as_array()
                .expect("summary messages")
                .last()
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
                .is_some_and(|text| text == COMPACTION_PROMPT)
        );
        assert!(upstream_body.get("reasoning_effort").is_none());
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn external_compaction_trigger_streams_one_emp_owned_checkpoint() {
        let upstream = chat_summary_upstream("stream checkpoint");
        let (_directory, server) = configured_server(&upstream.base_url());
        let request_body = serde_json::to_vec(&json!({
            "model": "demo/model",
            "stream": true,
            "input": [{
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "history"}]
            }, {"type":"compaction_trigger"}]
        }))
        .expect("request JSON");
        let response = post_stream(
            &server,
            "/v1/responses",
            &request_body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("Content-Type: text/event-stream\r\n"));
        assert!(!response.contains("Content-Length:"));
        assert!(response.contains("event: response.output_item.done\n"));
        assert!(response.contains("event: response.completed\n"));
        assert_eq!(response.matches("\"type\": \"compaction\"").count(), 3);

        let (_, _, upstream_body) = upstream.observed();
        assert!(!upstream_body.to_string().contains("compaction_trigger"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn native_checkpoint_switch_to_external_rebuilds_visible_codex_history() {
        let upstream = OneShotUpstream::start(json!({
            "id": "chat_upstream", "model": "upstream-model",
            "object": "chat.completion",
            "choices": [{"index":0,"message":{"role":"assistant","content":"continued"},"finish_reason":"stop"}]
        }));
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = canonical_root(&directory);
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "providers":[{"id":"demo","name":"Demo","base_url":upstream.base_url(),"protocol":"chat_completions","auth_mode":"api_key","api_key":"upstream-secret"}],
                "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream-model","enabled":true}]
            }))
            .unwrap(),
        )
        .unwrap();
        let thread_id = "01a00000-0000-7000-8000-000000000001";
        let turn_id = "01a00000-0000-7000-8000-000000000002";
        let rollout = root.join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":thread_id,"history_mode":"legacy"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"keep this constraint"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":"completed old work"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"compact"}}),
            json!({"type":"compacted","payload":{"message":""}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"compact"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":turn_id}}),
        ];
        std::fs::write(
            &rollout,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let database = rusqlite::Connection::open(root.join("state_5.sqlite")).unwrap();
        database.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)", []).unwrap();
        database
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'legacy', 'gpt-native')",
                rusqlite::params![thread_id, rollout.to_str().unwrap()],
            )
            .unwrap();
        drop(database);
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            "codex",
            root.join("auth.json"),
        )
        .expect("start history server");
        let body = serde_json::to_vec(&json!({
            "model":"demo/model",
            "stream":false,
            "input":[
                {"type":"compaction","encrypted_content":"native-opaque"},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"continue now"}]}
            ]
        })).unwrap();
        let metadata = format!("{{\"thread_id\":\"{thread_id}\",\"turn_id\":\"{turn_id}\"}}");
        let response = post(
            &server,
            "/v1/responses",
            &body,
            &[
                &session_cookie_header(&server),
                &format!("thread-id: {thread_id}"),
                &format!("x-codex-turn-metadata: {metadata}"),
            ],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let (_, _, upstream_body) = upstream.observed();
        let projected = upstream_body.to_string();
        assert!(projected.contains("keep this constraint"));
        assert!(projected.contains("completed old work"));
        assert!(projected.contains("continue now"));
        assert!(!projected.contains("native-opaque"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn long_to_short_external_switch_compacts_before_the_destination_request() {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind compaction upstream");
        let address = listener.local_addr().unwrap();
        let (sender, observed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().expect("accept compaction request");
                let (_, _, body) = receive_upstream_request(&mut stream);
                let wire = body.to_string();
                let summary = wire.contains("structured portable checkpoint")
                    || wire.contains("Merge the visible portable checkpoints");
                requests.push(body);
                let answer = if summary {
                    "checkpoint"
                } else {
                    "final answer"
                };
                let response_body = serde_json::to_vec(&json!({
                    "id":"chat","object":"chat.completion","model":"short-model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}]
                })).unwrap();
                stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                ).as_bytes()).unwrap();
                stream.write_all(&response_body).unwrap();
                if !summary {
                    sender.send(requests).unwrap();
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let root = canonical_root(&directory);
        let config = root.join("config.json");
        std::fs::write(&config, serde_json::to_vec_pretty(&json!({
            "providers":[{"id":"short","name":"Short","base_url":format!("http://{address}/v1"),"protocol":"chat_completions","auth_mode":"api_key","api_key":"key"}],
            "models":[{"id":"short/model","provider":"short","upstream_id":"short-model","enabled":true,
                "context_window":1200,"output_limit":64,
                "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}}]
        })).unwrap()).unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            "codex",
            root.join("auth.json"),
        )
        .unwrap();
        let body = serde_json::to_vec(&json!({
            "model":"short/model","stream":false,"max_output_tokens":64,
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"x".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"y".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"z".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"w".repeat(500)}]},
                {"type":"message","role":"user","content":[{"type":"input_text","text":"active request"}]}
            ]
        })).unwrap();
        let response = post(
            &server,
            "/v1/responses",
            &body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let requests = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            requests.len() >= 2,
            "summary request plus destination request"
        );
        let final_request = requests.last().unwrap().to_string();
        assert!(final_request.contains("checkpoint"));
        assert!(final_request.contains("active request"));
        assert!(!final_request.contains(&"x".repeat(500)));
        for summary in &requests[..requests.len() - 1] {
            assert_eq!(summary["stream"], false);
            assert!(!summary.to_string().contains("active request"));
        }
        server.shutdown().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn migration_export_and_import_cross_the_authenticated_http_boundary() {
        let source_directory = tempfile::tempdir().unwrap();
        let source_root = canonical_root(&source_directory);
        let source_config = source_root.join("config.json");
        std::fs::write(&source_config, serde_json::to_vec_pretty(&json!({
            "providers":[{"id":"demo","name":"Demo","base_url":"https://api.example.com/v1","protocol":"responses","auth_mode":"api_key","api_key":"secret"}],
            "models":[{"id":"demo/model","provider":"demo","upstream_id":"model","enabled":true}]
        })).unwrap()).unwrap();
        let source = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &source_config,
            "codex",
            source_root.join("auth.json"),
        )
        .unwrap();
        let export = post(
            &source,
            "/api/migration/export",
            br#"{"password":"12345678","groups":["external"]}"#,
            &[&session_cookie_header(&source)],
        );
        assert!(export.starts_with("HTTP/1.1 200 OK\r\n"), "{export}");
        assert!(export.contains("Content-Disposition: attachment; filename=\"EMP.emp\"\r\n"));
        let bundle = export.split_once("\r\n\r\n").unwrap().1.as_bytes();
        assert!(bundle.starts_with(b"EMP-MIGRATION\x01\n"));
        source.shutdown().unwrap();

        let target_directory = tempfile::tempdir().unwrap();
        let target_root = canonical_root(&target_directory);
        let target_config = target_root.join("config.json");
        std::fs::write(&target_config, b"{}").unwrap();
        let target = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &target_config,
            "codex",
            target_root.join("auth.json"),
        )
        .unwrap();
        let import_body = serde_json::to_vec(&json!({
            "password":"12345678",
            "bundle":STANDARD.encode(bundle)
        }))
        .unwrap();
        let imported = post(
            &target,
            "/api/migration/import",
            &import_body,
            &[&session_cookie_header(&target)],
        );
        assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
        let config = request(&target, "/api/config", &[&session_cookie_header(&target)]);
        assert!(config.contains("demo/model"));
        assert!(!config.contains("\"api_key\":\"secret\""));
        let stored = target.state.backend.config.lock().unwrap().clone();
        let provider = stored["providers"].as_array().unwrap()[0].clone();
        assert_eq!(
            provider_api_key(&provider, &target.state.backend.vault),
            "secret"
        );
        target.shutdown().unwrap();
    }

    #[test]
    fn account_import_and_delete_keep_credentials_managed_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let root = canonical_root(&directory);
        let config = root.join("config.json");
        std::fs::write(&config, b"{}").unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            "codex",
            root.join("auth.json"),
        )
        .unwrap();
        let imported = post(
            &server,
            "/api/accounts/import",
            &serde_json::to_vec(&json!({
                "id":"egg","name":"Egg","prefix":"egg","enabled":true,
                "auth_json":{"tokens":{"access_token":"account-secret","account_id":"account-id"}}
            }))
            .unwrap(),
            &[&session_cookie_header(&server)],
        );
        assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
        assert!(imported.contains("\"credential_set\":true"));
        assert!(!imported.contains("account-secret"));
        let stored = server.state.backend.config.lock().unwrap().clone();
        let auth_path = PathBuf::from(stored["accounts"][0]["auth_file"].as_str().unwrap());
        assert!(auth_path.is_file());
        assert!(auth_path.parent().unwrap().join("config.toml").is_file());
        assert_eq!(
            server
                .state
                .backend
                .vault
                .read_encrypted_json(&auth_path)
                .unwrap()["tokens"]["access_token"],
            "account-secret"
        );
        let removed = delete(
            &server,
            "/api/accounts/egg",
            &[&session_cookie_header(&server)],
        );
        assert!(removed.starts_with("HTTP/1.1 200 OK\r\n"), "{removed}");
        assert!(!auth_path.exists());
        assert!(!auth_path.parent().unwrap().join("config.toml").exists());
        assert!(
            server.state.backend.config.lock().unwrap()["accounts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        server.shutdown().unwrap();
    }

    #[test]
    fn native_search_forwards_raw_json_with_the_best_available_login() {
        let upstream = OneShotUpstream::start(json!({"data":[{"title":"result"}]}));
        let directory = tempfile::tempdir().unwrap();
        let root = canonical_root(&directory);
        let config = root.join("config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "codex_base_url":format!("http://{}/backend",upstream.address),
                "subscription_search":{"enabled":true,"account_id":""}
            }))
            .unwrap(),
        )
        .unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            "codex",
            root.join("missing-auth.json"),
        )
        .unwrap();
        let response = post(
            &server,
            "/v1/alpha/search",
            br#"{"query":"codex"}"#,
            &[
                &session_cookie_header(&server),
                "Authorization: Bearer caller-token",
                "chatgpt-account-id: caller-account",
            ],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("\"title\":\"result\""));
        let (path, headers, body) = upstream.observed();
        assert_eq!(path, "/backend/alpha/search");
        assert_eq!(headers["authorization"], "Bearer caller-token");
        assert_eq!(headers["chatgpt-account-id"], "caller-account");
        assert_eq!(body, json!({"query":"codex"}));
        server.shutdown().unwrap();
    }

    fn upstream_sse(events: &[Value]) -> Vec<u8> {
        let mut wire = Vec::new();
        for event in events {
            wire.extend_from_slice(b"data: ");
            wire.extend_from_slice(
                serde_json::to_string(event)
                    .expect("upstream SSE JSON")
                    .as_bytes(),
            );
            wire.extend_from_slice(b"\n\n");
        }
        wire.extend_from_slice(b"data: [DONE]\n\n");
        wire
    }

    fn assert_stream_protocol(
        protocol: &str,
        auth_mode: &str,
        expected_path: &str,
        events: &[Value],
    ) {
        let upstream = OneShotUpstream::start_sse(vec![upstream_sse(events)]);
        let (_directory, server) =
            configured_protocol_server(&upstream.base_url(), protocol, auth_mode);
        let request_body = serde_json::to_vec(&json!({
            "model": "demo/model",
            "input": [{
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }],
            "stream": true
        }))
        .expect("request JSON");
        let response = post_stream(
            &server,
            "/v1/responses",
            &request_body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("Content-Type: text/event-stream\r\n"));
        assert!(!response.contains("Content-Length:"));
        assert!(response.contains("event: response.created\n"), "{response}");
        assert!(
            response.contains("event: response.output_text.delta\n"),
            "{response}"
        );
        assert!(
            response.contains("event: response.completed\n"),
            "{response}"
        );
        let (path, headers, upstream_body) = upstream.observed();
        assert_eq!(path, expected_path);
        if auth_mode == "anthropic_api_key" {
            assert_eq!(headers["x-api-key"], "upstream-secret");
        } else {
            assert_eq!(headers["authorization"], "Bearer upstream-secret");
        }
        assert_eq!(upstream_body["model"], "upstream-model");
        assert_eq!(upstream_body["stream"], true);
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn streamed_external_protocols_cross_the_real_server_boundary() {
        let chat = [
            json!({"id":"chat_upstream","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
            json!({"id":"chat_upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
        ];
        assert_stream_protocol("chat_completions", "api_key", "/v1/chat/completions", &chat);

        let anthropic = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ];
        assert_stream_protocol(
            "anthropic_messages",
            "anthropic_api_key",
            "/v1/messages",
            &anthropic,
        );

        let response = json!({
            "id":"responses_upstream", "object":"response", "status":"completed",
            "model":"upstream-model",
            "output":[{"id":"msg_visible","type":"message","status":"completed",
                "role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}],
            "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
        });
        let responses = [
            json!({"type":"response.created","response":{"id":"responses_upstream","object":"response","status":"in_progress","model":"upstream-model","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_visible","type":"message","status":"in_progress","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","item_id":"msg_visible","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_visible","output_index":0,"content_index":0,"delta":"answer"}),
            json!({"type":"response.output_text.done","item_id":"msg_visible","output_index":0,"content_index":0,"text":"answer"}),
            json!({"type":"response.content_part.done","item_id":"msg_visible","output_index":0,"content_index":0,"part":{"type":"output_text","text":"answer","annotations":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_visible","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}}),
            json!({"type":"response.completed","response":response}),
        ];
        assert_stream_protocol("responses", "api_key", "/v1/responses", &responses);
    }

    #[test]
    fn streaming_flushes_before_upstream_eof() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
        let address = listener.local_addr().expect("upstream address");
        let first_event = json!({
            "id":"chat_upstream",
            "choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]
        });
        let first = format!(
            "data: {}\n\n",
            serde_json::to_string(&first_event).expect("first upstream event")
        )
        .into_bytes();
        let last = upstream_sse(&[json!({
            "id":"chat_upstream",
            "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
        })]);
        let content_length = first.len() + last.len();
        let (first_sent, first_ready) = mpsc::sync_channel(1);
        let (release, released) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream");
            let _ = receive_upstream_request(&mut stream);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .expect("write upstream response head");
            stream.write_all(&first).expect("write first SSE event");
            stream.flush().expect("flush first SSE event");
            first_sent.send(()).expect("announce first SSE event");
            released
                .recv_timeout(Duration::from_secs(3))
                .expect("release terminal SSE event");
            stream.write_all(&last).expect("write terminal SSE event");
        });
        let (_directory, server) = configured_server(&format!("http://{address}/v1"));
        let body = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":true,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("request JSON");
        let mut downstream = open_post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&session_cookie_header(&server)],
        );
        first_ready
            .recv_timeout(Duration::from_secs(2))
            .expect("upstream first event");
        downstream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("downstream timeout");
        let mut response = Vec::new();
        while !response
            .windows(b"event: response.output_text.delta\n".len())
            .any(|window| window == b"event: response.output_text.delta\n")
        {
            let mut chunk = [0_u8; 4096];
            let count = downstream.read(&mut chunk).expect("incremental SSE read");
            assert!(count > 0, "downstream ended before visible output");
            response.extend_from_slice(&chunk[..count]);
        }
        release.send(()).expect("release terminal SSE event");
        downstream
            .read_to_end(&mut response)
            .expect("finish downstream SSE");
        let response = String::from_utf8(response).expect("UTF-8 SSE response");
        assert!(response.contains("event: response.completed\n"));
        server.shutdown().expect("shutdown");
        worker.join().expect("join upstream");
    }

    #[test]
    fn downstream_disconnect_cancels_a_waiting_upstream_stream() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
        let address = listener.local_addr().expect("upstream address");
        let (head_sent, head_ready) = mpsc::sync_channel(1);
        let (closed_sender, closed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept upstream");
            let _ = receive_upstream_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .expect("write upstream response head");
            stream.flush().expect("flush upstream response head");
            head_sent.send(()).expect("announce upstream head");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("upstream close timeout");
            let mut byte = [0_u8; 1];
            let closed_by_emp = match stream.read(&mut byte) {
                Ok(0) => true,
                Err(error) => matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ),
                Ok(_) => false,
            };
            closed_sender
                .send(closed_by_emp)
                .expect("report upstream cancellation");
        });
        let (_directory, server) = configured_server(&format!("http://{address}/v1"));
        let body = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":true,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("request JSON");
        let downstream = open_post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&session_cookie_header(&server)],
        );
        head_ready
            .recv_timeout(Duration::from_secs(2))
            .expect("upstream response head");
        drop(downstream);
        assert!(
            closed
                .recv_timeout(Duration::from_secs(3))
                .expect("upstream cancellation result"),
            "EMP kept the upstream stream open after its downstream disconnected"
        );
        server.shutdown().expect("shutdown");
        worker.join().expect("join upstream");
    }

    #[test]
    fn stream_errors_keep_pre_and_post_output_boundaries() {
        let upstream = OneShotUpstream::start_error(
            429,
            Some(6),
            json!({"error":{"message":"provider detail must not escape"}}),
        );
        let (_directory, server) = configured_server(&upstream.base_url());
        let body = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":true,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("request JSON");
        let response = post(
            &server,
            "/v1/responses",
            &body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 429 Too Many Requests\r\n"));
        assert!(response.contains("Retry-After: 6\r\n"));
        assert!(response.contains("\"type\":\"rate_limit\""));
        assert!(response.contains("\"code\":\"rate_limit_exceeded\""));
        assert!(!response.contains("provider detail must not escape"));
        server.shutdown().expect("shutdown");

        let partial = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id":"chat_upstream",
                "choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]
            }))
            .expect("partial upstream event")
        )
        .into_bytes();
        let upstream = OneShotUpstream::start_sse(vec![partial]);
        let (_directory, server) = configured_server(&upstream.base_url());
        let response = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("event: response.output_text.delta\n"));
        assert!(response.contains("event: response.failed\n"));
        assert!(response.contains("\"status\": 502"));
        assert!(!response.contains("event: response.completed\n"));
        server.shutdown().expect("shutdown");
    }

    #[test]
    fn auto_protocol_falls_back_only_after_explicit_endpoint_rejection() {
        let complete_body = serde_json::to_vec(&json!({
            "id":"responses_upstream", "object":"response", "status":"completed",
            "model":"upstream-model",
            "output":[{"id":"msg_visible","type":"message","status":"completed",
                "role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}],
            "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
        }))
        .expect("complete upstream body");
        let (base_url, paths, worker) = fallback_upstream("application/json", complete_body);
        let (directory, server) = configured_protocol_server(&base_url, "auto", "api_key");
        let complete_request = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":false,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("complete request JSON");
        let response = post(
            &server,
            "/v1/responses",
            &complete_request,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("\"text\":\"answer\""));
        assert_eq!(
            [
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            ],
            ["/v1/chat/completions", "/v1/responses"]
        );
        assert_saved_protocol_observation(&directory, &server, "responses");
        server.shutdown().expect("shutdown");
        worker.join().expect("join complete fallback upstream");

        let terminal = json!({
            "id":"responses_upstream", "object":"response", "status":"completed",
            "model":"upstream-model",
            "output":[{"id":"msg_visible","type":"message","status":"completed",
                "role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}],
            "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
        });
        let stream_body = upstream_sse(&[
            json!({"type":"response.created","response":{"id":"responses_upstream","object":"response","status":"in_progress","model":"upstream-model","output":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_visible","output_index":0,"content_index":0,"delta":"answer"}),
            json!({"type":"response.completed","response":terminal}),
        ]);
        let (base_url, paths, worker) = fallback_upstream("text/event-stream", stream_body);
        let (directory, server) = configured_protocol_server(&base_url, "auto", "api_key");
        let stream_request = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":true,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("stream request JSON");
        let response = post_stream(
            &server,
            "/v1/responses",
            &stream_request,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("event: response.output_text.delta\n"));
        assert!(response.contains("event: response.completed\n"));
        assert_eq!(
            [
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            ],
            ["/v1/chat/completions", "/v1/responses"]
        );
        assert_saved_protocol_observation(&directory, &server, "responses");
        server.shutdown().expect("shutdown");
        worker.join().expect("join stream fallback upstream");
    }

    #[test]
    fn external_pre_output_retry_is_single_and_route_local() {
        let complete_body = serde_json::to_vec(&json!({
            "id":"chat_upstream", "model":"upstream-model", "object":"chat.completion",
            "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
        }))
        .expect("complete Chat body");
        let (base_url, paths, worker) =
            two_attempt_upstream(429, Some(0), "application/json", complete_body);
        let (_directory, server) = configured_server(&base_url);
        let complete_request = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":false,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("complete request JSON");
        let response = post(
            &server,
            "/v1/responses",
            &complete_request,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("\"text\":\"answer\""));
        assert_eq!(
            [
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            ],
            ["/v1/chat/completions", "/v1/chat/completions"]
        );
        server.shutdown().expect("shutdown");
        worker.join().expect("join complete retry upstream");

        let stream_body = upstream_sse(&[
            json!({"id":"chat_upstream","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
            json!({"id":"chat_upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
        ]);
        let (base_url, paths, worker) =
            two_attempt_upstream(429, Some(0), "text/event-stream", stream_body);
        let (_directory, server) = configured_server(&base_url);
        let stream_request = serde_json::to_vec(&json!({
            "model":"demo/model", "stream":true,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
        }))
        .expect("stream request JSON");
        let response = post_stream(
            &server,
            "/v1/responses",
            &stream_request,
            &[&session_cookie_header(&server)],
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("event: response.completed\n"));
        assert_eq!(
            [
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
                paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            ],
            ["/v1/chat/completions", "/v1/chat/completions"]
        );
        server.shutdown().expect("shutdown");
        worker.join().expect("join stream retry upstream");
    }

    #[test]
    fn stream_boundary_helpers_match_live_python() {
        fn semantic_frames(value: &Value) -> Vec<Value> {
            value
                .as_array()
                .expect("frame array")
                .iter()
                .map(|frame| {
                    let frame = frame.as_str().expect("frame string");
                    let (event, data) = frame
                        .strip_prefix("event: ")
                        .and_then(|frame| frame.split_once("\ndata: "))
                        .expect("SSE event and data lines");
                    let data = data.strip_suffix("\n\n").expect("SSE terminator");
                    json!({
                        "event": event,
                        "data": serde_json::from_str::<Value>(data).expect("SSE JSON data"),
                    })
                })
                .collect()
        }

        let events = [
            json!({"type":"response.created","response":{"status":"in_progress","output":[]}}),
            json!({"type":"response.output_text.delta","delta":"回答, key: value"}),
            json!({"type":"response.output_item.added","item":{"id":"call_1","type":"function_call"}}),
        ];
        let rust = json!({
            "frames": events.iter().map(|event| {
                String::from_utf8(sse_frame(event["type"].as_str().unwrap(), event).unwrap()).unwrap()
            }).collect::<Vec<_>>(),
            "activity": events.iter().map(|event| {
                let (output, tool) = stream_event_activity(event);
                json!([output, tool])
            }).collect::<Vec<_>>(),
        });
        let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
            return;
        };
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = r#"
import json
from easy_multi_provider.stream_adapters import _sse_frame
from easy_multi_provider.transport_failures import event_activity
events = [
    {"type":"response.created","response":{"status":"in_progress","output":[]}},
    {"type":"response.output_text.delta","delta":"回答, key: value"},
    {"type":"response.output_item.added","item":{"id":"call_1","type":"function_call"}},
]
print(json.dumps({
    "frames":[_sse_frame(event["type"], event).decode() for event in events],
    "activity":[list(event_activity(event)) for event in events],
}, ensure_ascii=False))
"#;
        let output = Command::new(python)
            .arg("-c")
            .arg(script)
            .current_dir(root)
            .output()
            .expect("spawn Python stream boundary oracle");
        assert!(
            output.status.success(),
            "Python stream boundary oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let python: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
        assert_eq!(rust["activity"], python["activity"]);
        assert_eq!(
            semantic_frames(&rust["frames"]),
            semantic_frames(&python["frames"])
        );
    }

    #[test]
    fn response_body_errors_keep_the_python_status_boundary() {
        let (_directory, server) = test_server();
        let cookie = session_cookie_header(&server);
        let wrong_type = post(
            &server,
            "/v1/responses",
            b"{}",
            &[&cookie, "Content-Type: text/plain"],
        );
        assert!(wrong_type.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(wrong_type.contains("Content-Type must be application/json"));

        let non_object = post(&server, "/v1/responses", b"[]", &[&cookie]);
        assert!(non_object.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(non_object.contains("request body must be a JSON object"));

        let missing_model = post(&server, "/v1/responses", b"{}", &[&cookie]);
        assert!(missing_model.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(missing_model.contains("request.model is required"));

        let unknown_stream = post(
            &server,
            "/v1/responses",
            br#"{"model":"anything","stream":true}"#,
            &[&cookie],
        );
        assert!(unknown_stream.starts_with("HTTP/1.1 404 Not Found\r\n"));
        server.shutdown().expect("shutdown");

        let (_directory, server) = configured_server("http://127.0.0.1:9/v1");
        let cookie = session_cookie_header(&server);
        let stream = post(
            &server,
            "/v1/responses",
            br#"{"model":"demo/model","stream":true}"#,
            &[&cookie],
        );
        assert!(stream.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
        assert!(stream.contains("network"));
        server.shutdown().expect("shutdown");
    }
}
