//! EMP native executable and local management HTTP surface.
//!
//! This bounded Rust slice serves the unchanged Web UI, the existing health
//! check, and complete or streamed external `/v1/responses` requests. Native
//! accounts, compact and management APIs remain later vertical slices.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_core::{ResolvedRoute, RouteResolutionError, resolve_route_without_catalog};
use emp_router::{
    ExternalRouter, ExternalStream, ProjectionIds, RouterError, RouterErrorKind,
    StreamResponseEvent,
};
use emp_state::{
    ConfigError, FilesystemError, VaultStore, WEB_SESSION_TOKEN_BYTES, WebSession, WebSessionError,
    config_path, load_configuration, load_or_create_web_session, provider_api_key,
    web_session_path,
};
use emp_transport::{
    ContentDecodeError, FailureClass, HttpClient, HttpClientPolicy, ProxyEnvironment, ProxyPolicy,
    RequestCapacityError, RequestLimits, RequestLimitsConfig, RequestLimitsError, TimeoutPolicy,
    TransportKind, decode_content, normalize_error_class, public_failure_message,
};
use serde_json::Value;
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};

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
    let length = usize::try_from(length).map_err(|_| {
        BodyError::Capacity(RequestCapacityError {
            limit: RequestLimitsConfig::default().maximum,
            decoded: false,
            reason: emp_transport::RequestCapacityReason::HardLimit,
            available_bytes: 0,
            required_memory_bytes: 0,
            memory_total_bytes: 0,
            memory_used_bytes: 0,
            memory_used_percent: None,
        })
    })?;
    let mut budget = state.backend.request_limits.request(TransportKind::Http);
    budget.ensure(length).map_err(BodyError::Capacity)?;
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
        RequestLimitsConfig::default().maximum,
        Some(&mut budget),
    )
    .map_err(BodyError::Decode)?;
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

fn write_stream_head(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )
}

fn write_stream_frames(stream: &mut TcpStream, frames: &[Vec<u8>]) -> std::io::Result<()> {
    for frame in frames {
        stream.write_all(frame)?;
    }
    stream.flush()
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
    let mut upstream = state
        .backend
        .runtime
        .block_on(router.open_stream(route, body, incoming, ids))
        .map_err(|error| pre_output_router_error_response(&error))?;
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
            StreamPoll::Disconnected => return Ok(()),
            StreamPoll::Event(Ok(Some(event))) => event,
            StreamPoll::Event(Ok(None)) => return Ok(()),
            StreamPoll::Event(Err(error)) if !started => {
                return Err(pre_output_router_error_response(&error));
            }
            StreamPoll::Event(Err(error)) => {
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => return Ok(()),
                };
                let failure = stream_failure_value(&error, &response_id);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = write_stream_frames(downstream, &[frame]);
                }
                return Ok(());
            }
        };
        let terminal = terminal_stream_event(&event.body);
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
            Err(_) => return Ok(()),
        };
        if started {
            if write_stream_frames(downstream, &[frame]).is_err() || terminal {
                return Ok(());
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
                return Ok(());
            }
            return Ok(());
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
                return Ok(());
            }
            started = true;
            pending.clear();
            if terminal {
                return Ok(());
            }
        }
    }
}

enum ResponsesRequestResult {
    Buffered(Vec<u8>),
    Streamed,
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
    let body = match read_json_body(stream, request, body_prefix, state) {
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
    let mut config = state.backend.config.clone();
    hydrate_provider_keys(&mut config, &state.backend.vault);
    let route = match resolve_route_without_catalog(&config, model) {
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
    let incoming = BTreeMap::from([("X-EMP-Request-ID".to_owned(), request_id)]);
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return match serve_external_stream(stream, state, &route, &body, &incoming, &ids) {
            Ok(()) => ResponsesRequestResult::Streamed,
            Err(response) => ResponsesRequestResult::Buffered(response),
        };
    }
    let router = ExternalRouter::new(&state.backend.client);
    match state
        .backend
        .runtime
        .block_on(router.execute_complete(&route, &body, &incoming, &ids))
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
            ResponsesRequestResult::Buffered(response(
                &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
                &result.content_type,
                &body,
                &[],
            ))
        }
        Err(error) => ResponsesRequestResult::Buffered(router_error_response(error)),
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
            if request.method == RequestMethod::Post && request.raw_path() == "/v1/responses" =>
        {
            match responses_request(&mut stream, request, raw.body_prefix, state, system_now()) {
                ResponsesRequestResult::Buffered(response) => Some(response),
                ResponsesRequestResult::Streamed => None,
            }
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

fn route_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    route_request_at(request, state, system_now())
}

fn route_request_at(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let path = request.raw_path();
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
    config: Value,
    vault: VaultStore,
    client: HttpClient,
    runtime: Runtime,
    request_limits: Arc<RequestLimits>,
    native_auth_path: PathBuf,
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
        if !is_loopback(host) {
            return Err(AppError::HostNotLoopback);
        }
        let now = system_now();
        let session_path = web_session_path(config_path)?;
        let session =
            load_or_create_web_session(&session_path, now).map_err(AppError::WebSession)?;
        let config = load_configuration(Some(config_path))?;
        let state_root = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("state");
        let vault = VaultStore::from_environment(&state_root.join("master.key"))?;
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
        let backend = BackendState {
            config,
            vault,
            client,
            runtime,
            request_limits,
            native_auth_path: codex_auth_path(),
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
    use super::*;
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
        let length = headers["content-length"]
            .parse::<usize>()
            .expect("upstream Content-Length");
        let mut body = raw.body_prefix;
        while body.len() < length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).expect("read upstream body");
            assert!(count > 0, "upstream body ended early");
            body.extend_from_slice(&chunk[..count]);
        }
        body.truncate(length);
        let body = serde_json::from_slice(&body).expect("upstream request JSON");
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
        assert!(api.starts_with("HTTP/1.1 404 Not Found\r\n"));
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
            Some(4),
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
        assert!(response.contains("Retry-After: 4\r\n"));
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
