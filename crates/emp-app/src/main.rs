//! EMP native executable and local management HTTP surface.
//!
//! This bounded Rust slice serves the unchanged Web UI and the existing health
//! check with the Python-compatible management bootstrap contract. Production
//! API routes remain absent; only their authentication boundary is present so
//! the binary cannot be mistaken for a complete router port.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use emp_state::{
    WEB_SESSION_TOKEN_BYTES, WebSession, WebSessionError, config_path, load_or_create_web_session,
    web_session_path,
};

pub const VERSION: &str = "0.11.6";

/// Embedded directly from the existing Python package so Web UI bytes cannot
/// drift during the rewrite.
pub const WEB_INDEX_BYTES: &[u8] = include_bytes!("../../../easy_multi_provider/web/index.html");

/// Exact compact JSON body observed in the Python server tests.
pub const HEALTH_JSON_BYTES: &[u8] = b"{\"status\":\"ok\"}";

const LOGIN_HTML: &str = r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>登录 EMP</title><body style="font-family:system-ui;max-width:36rem;margin:12vh auto;padding:24px;line-height:1.7"><h1>请从 EMP 打开管理页</h1><p>此浏览器尚未登录，或登录已过期。</p><p>请打开 EMP 启动时自动弹出的网页；也可以使用终端中 Open in browser 后的完整链接。</p><p>登录有效期为 30 天，期间重启 EMP 无需重新登录。</p></body></html>"#;

const LOGIN_HTML_BYTES: &[u8] = LOGIN_HTML.as_bytes();

const MAX_REQUEST_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub enum AppError {
    HostNotLoopback,
    Io(std::io::Error),
    ServerStopped,
    RandomUnavailable,
    WebSession(WebSessionError),
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

#[derive(Clone, Copy)]
struct Request<'a> {
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

fn read_request(stream: &mut TcpStream) -> Option<String> {
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
        if request.len() > MAX_REQUEST_BYTES {
            return None;
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).ok()
}

fn request_target(request: &str) -> Option<&str> {
    let request_line = request.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return None;
    }
    parts.next()
}

fn parse_request(request: &str) -> Option<Request<'_>> {
    let headers_end = request.find("\r\n\r\n")?;
    let target = request_target(request)?;
    Some(Request {
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

fn handle_connection(mut stream: TcpStream, state: &ServerState) {
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let raw = match read_request(&mut stream) {
        Some(raw) => raw,
        None => {
            let _ = stream.write_all(&bad_request_response());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    let response = match parse_request(&raw) {
        Some(request) => route_request(request, state),
        None => bad_request_response(),
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
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
    port: u16,
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
        Self::start_with_session(host, port, session_path, session)
    }

    fn start_with_session(
        host: IpAddr,
        port: u16,
        session_path: PathBuf,
        session: WebSession,
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

    use emp_state::WEB_SESSION_TOKEN_LENGTH;
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
}
