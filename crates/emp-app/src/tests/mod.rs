//! Tests.
use crate::api::quota::QUOTA_EVENT_SLOT_LIMIT;
use base64::engine::general_purpose::STANDARD;

use crate::cli::Cli;
use crate::cli::parse_cli;
use crate::http::auth::parse_session_cookie;
use crate::http::auth::valid_caller_authorization;
use crate::http::request::RequestHead;
use crate::http::request::parse_request;
use crate::http::request::read_request_head;
use crate::http::response::status_text;
use crate::http::routes::route_request_at;
use crate::lifecycle::ServerHandle;
use crate::services::accounts::notify_quota_update;
use crate::services::compaction::COMPACTION_PROMPT;
use crate::services::events::sse_frame;
use crate::services::events::stream_event_activity;
#[cfg(unix)]
use crate::services::quota::QuotaSampleCounts;
#[cfg(unix)]
use crate::services::quota::sample_quotas_once;
use crate::web::WEB_INDEX_BYTES;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use emp_state::WEB_SESSION_TOKEN_LENGTH;
use emp_state::load_configuration;
use emp_state::provider_api_key;
use emp_state::save_configuration;
use emp_transport::WebSocketConnection;
use emp_transport::websocket_accept;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Read;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

mod auto_review_contract;
mod cancellation_contract;
mod catalog_api_contract;
mod config_api_contract;
mod conversation_http_contract;
mod conversation_switch_contract;
mod integration_sideband_contract;
mod management_http_contract;
mod native_api_contract;
mod performance_contract;
mod quota_management_contract;
mod quota_workspace_contract;
mod realtime_contract;
mod realtime_sideband_contract;
mod session_boundary_contract;
mod stream_boundary_contract;
mod websocket_capacity_contract;

fn canonical_root(directory: &TempDir) -> PathBuf {
    directory
        .path()
        .canonicalize()
        .expect("canonical temporary root")
}

fn complete_response(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("response timeout");
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).expect("read response");
        assert!(count > 0, "response ended before Content-Length bytes");
        response.extend_from_slice(&buffer[..count]);
        let Some(separator) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
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
        .set_read_timeout(Some(Duration::from_secs(20)))
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
            format!("GET {target} HTTP/1.1\r\n{host}{full_headers}\r\nConnection: close\r\n\r\n")
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

fn receive_upstream_request(stream: &mut TcpStream) -> (String, BTreeMap<String, String>, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("upstream timeout");
    let raw = read_request_head(stream).expect("upstream request head");
    receive_upstream_request_from_head(stream, raw)
}

fn receive_upstream_request_from_head(
    stream: &mut TcpStream,
    raw: RequestHead,
) -> (String, BTreeMap<String, String>, Value) {
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

    fn start_wire(
        status: u16,
        content_type: &'static str,
        retry_after: Option<u64>,
        chunks: Vec<Vec<u8>>,
    ) -> Self {
        Self::start_repeated_wire(status, content_type, retry_after, 1, chunks)
    }

    /// Answers every connection identically; `connections` bounds the
    /// accept loop so callers can exercise the full retry budget.
    fn start_repeated_wire(
        status: u16,
        content_type: &'static str,
        retry_after: Option<u64>,
        connections: usize,
        chunks: Vec<Vec<u8>>,
    ) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
        let address = listener.local_addr().expect("upstream address");
        let (sender, observed) = mpsc::sync_channel(8);
        let worker = thread::spawn(move || {
            for _ in 0..connections {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let (path, headers, body) = receive_upstream_request(&mut stream);
                let _ = sender.send((path, headers, body));
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
                for chunk in chunks.clone() {
                    stream
                        .write_all(&chunk)
                        .expect("write upstream response body");
                    stream.flush().expect("flush upstream response body");
                }
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

/// Serves `first_status` until the client stops retrying, then success.
/// Accepts up to three connections to cover the full retry budget; unused
/// accepts are drained by `Drop` of the test's server handle.
fn two_attempt_upstream(
    first_status: u16,
    retry_after: Option<u64>,
    content_type: &'static str,
    success_body: Vec<u8>,
) -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fallback upstream");
    let address = listener.local_addr().expect("fallback upstream address");
    let (path_sender, paths) = mpsc::sync_channel(4);
    let worker = thread::spawn(move || {
        for attempt in 0..3 {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let (path, _, _) = receive_upstream_request(&mut stream);
            let _ = path_sender.send(path);
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
            if attempt >= 1 {
                // Success ends the scenario; further accepts would hang join.
                break;
            }
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

fn assert_saved_protocol_observation(directory: &TempDir, server: &ServerHandle, expected: &str) {
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
    let config = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config lock");
    assert_eq!(config["providers"][0]["resolved_protocol"], expected);
    assert_eq!(
        provider_api_key(
            &config["providers"][0],
            &server.state.backend.configuration.vault
        ),
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
