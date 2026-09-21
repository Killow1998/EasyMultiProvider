//! EMP native executable for the first Rust foundation slice.
//!
//! This development surface deliberately has only the unchanged Web UI and the
//! existing health check.  Production API routes are absent so the binary
//! cannot be mistaken for a complete router port.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub const VERSION: &str = "0.11.6";

/// Embedded directly from the existing Python package so Web UI bytes cannot
/// drift during the rewrite.
const WEB_INDEX_BYTES: &[u8] = include_bytes!("../../../easy_multi_provider/web/index.html");

/// Exact compact JSON body observed in the Python server tests.
pub const HEALTH_JSON_BYTES: &[u8] = b"{\"status\":\"ok\"}";

const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// Errors from the bounded application surface.
#[derive(Debug)]
pub enum AppError {
    HostNotLoopback,
    Io(std::io::Error),
    ServerStopped,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostNotLoopback => {
                formatter.write_str("host must be 127.0.0.1 for local-only management")
            }
            Self::Io(error) => write!(formatter, "{error}"),
            Self::ServerStopped => formatter.write_str("server task stopped before shutdown"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Supported first-slice commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cli {
    Version,
    Serve { host: IpAddr, port: u16 },
}

/// Parse only the commands implemented by this milestone.
pub fn parse_cli<I>(arguments: I) -> Result<Cli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let Some(command) = arguments.next() else {
        return Err("usage: EMP [--version] | serve --host HOST --port PORT".to_string());
    };
    if command == "--version" {
        return Ok(Cli::Version);
    }
    if command != "serve" {
        return Err(format!("unknown command: {command}"));
    }

    let mut host = None;
    let mut port = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
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
    Ok(Cli::Serve { host, port })
}

/// Preserve the Python server's explicit local-only rule.
pub fn is_loopback(host: IpAddr) -> bool {
    host == IpAddr::V4(Ipv4Addr::LOCALHOST)
        || matches!(host, IpAddr::V6(address) if address.is_loopback())
}

fn response(status_line: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = Vec::with_capacity(body.len() + 192);
    let _ = write!(
        response,
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    response.extend_from_slice(body);
    response
}

fn health_response() -> Vec<u8> {
    response("HTTP/1.1 200 OK", "application/json", HEALTH_JSON_BYTES)
}

fn ui_response() -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "text/html; charset=utf-8",
        WEB_INDEX_BYTES,
    )
}

fn not_found_response() -> Vec<u8> {
    response(
        "HTTP/1.1 404 Not Found",
        "application/json",
        b"{\"error\":{\"message\":\"not found\"}}",
    )
}

fn bad_request_response() -> Vec<u8> {
    response(
        "HTTP/1.1 400 Bad Request",
        "application/json",
        b"{\"error\":{\"message\":\"invalid HTTP request\"}}",
    )
}

fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = [0_u8; 1024];
    let mut request = Vec::new();
    loop {
        let count = stream.read(&mut buffer).ok()?;
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

fn request_path(request: &str) -> Option<&str> {
    let request_line = request.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return None;
    }
    parts.next()
}

fn handle_connection(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let request = match read_request(&mut stream) {
        Some(request) => request,
        None => {
            let _ = stream.write_all(&bad_request_response());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    let response = match request_path(&request) {
        Some("/") => ui_response(),
        Some("/healthz") => health_response(),
        _ => not_found_response(),
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
    // Finish the response with a TCP FIN before dropping the socket. macOS can
    // otherwise surface a reset to a client that is reading through EOF.
    let _ = stream.shutdown(Shutdown::Write);
}

/// A running listener with explicit, testable shutdown ownership.
pub struct ServerHandle {
    local_addr: SocketAddr,
    state: Arc<ServerState>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

struct ServerState {
    listener: TcpListener,
    shutdown: Arc<AtomicBool>,
}

impl ServerHandle {
    /// Bind a loopback listener and start worker threads.
    pub fn start(host: IpAddr, port: u16) -> Result<Self, AppError> {
        if !is_loopback(host) {
            return Err(AppError::HostNotLoopback);
        }
        let listener = TcpListener::bind((host, port))?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(ServerState { listener, shutdown });
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
                            let _ = thread::Builder::new()
                                .name("emp-request".to_string())
                                .spawn(move || handle_connection(stream));
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

    /// Stop accepting and wait for the accept worker to exit.  Already
    /// accepted request threads are short-lived and bounded.
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

/// Start the server, print an explicit readiness line, and block until the
/// process is terminated.
pub fn run_server(host: IpAddr, port: u16) -> Result<(), AppError> {
    let server = ServerHandle::start(host, port)?;
    let local_addr = server.local_addr();
    println!("EMP listening on http://{local_addr}");
    println!("Shutdown: terminate the process (SIGINT/SIGTERM where supported)");
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
        Cli::Serve { host, port } => {
            run_server(host, port).map_err(|error| error.to_string())?;
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

    #[test]
    fn cli_rejects_non_loopback_host_before_starting() {
        let error = parse_cli([
            "serve".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
            "--port".to_string(),
            "0".to_string(),
        ])
        .expect_err("non-loopback host must fail");
        assert_eq!(error, AppError::HostNotLoopback.to_string());
    }

    #[test]
    fn health_contract_is_exact() {
        let server = ServerHandle::start(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .expect("test server should start");
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        assert_eq!(
            response.rsplit("\r\n\r\n").next().unwrap(),
            "{\"status\":\"ok\"}"
        );
        drop(stream);
        server.shutdown().expect("graceful shutdown");
    }

    #[test]
    fn root_serves_the_existing_web_ui_bytes_exactly() {
        let server = ServerHandle::start(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
            .expect("test server should start");
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("write request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        let separator = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response header terminator");
        assert_eq!(&response[separator + 4..], WEB_INDEX_BYTES);
        drop(stream);
        server.shutdown().expect("graceful shutdown");
    }
}
