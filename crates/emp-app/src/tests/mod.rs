//! Tests.
#[cfg(feature = "hotpath")]
mod hotpath_profile;
use crate::api::quota::QUOTA_EVENT_SLOT_LIMIT;
use base64::engine::general_purpose::STANDARD;
use emp_test_support::{Deadline, test_timeout};

use crate::cli::Cli;
use crate::cli::parse_cli;
use crate::http::auth::valid_caller_authorization;
use crate::http::request::RequestHead;
use crate::http::request::parse_request;
use crate::http::request::read_request_head;
use crate::http::response::status_text;
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
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

mod activity_contract;
mod activity_websocket_contract;
mod aggregate_contract;
mod auto_review_contract;
mod cancellation_contract;
mod catalog_api_contract;
mod claude_availability_contract;
#[cfg(unix)]
mod claude_local_login_contract;
mod collaboration_contract;
mod config_api_contract;
mod conversation_http_contract;
mod conversation_switch_contract;
mod diagnostics;
mod gateway_attribution;
mod gateway_contract;
mod history_diagnostic_contract;
mod integration_sideband_contract;
mod internal_events_contract;
pub(crate) use internal_events_contract::live_journal;
mod management_http_contract;
mod management_operation_contract;
mod memory_routing_contract;
mod native_api_contract;
#[cfg(unix)]
pub(crate) use native_api_contract::fixture_tls;
#[cfg(unix)]
mod native_login_contract;
mod other_auth_contract;
mod performance_contract;
mod quota_management_contract;
mod quota_rotation_contract;
mod quota_workspace_contract;
pub(crate) mod realtime_contract;
mod realtime_sideband_contract;
mod request_observation_contract;
mod response_synthesis_contract;
mod retry_advice_contract;
mod session_boundary_contract;
mod stream_boundary_contract;
mod websocket_capacity_contract;
#[cfg(unix)]
pub(crate) use websocket_capacity_contract::wait_for;

fn canonical_root(directory: &TempDir) -> PathBuf {
    directory
        .path()
        .canonicalize()
        .expect("canonical temporary root")
}

/// Existing imported-account fixtures must own a key before startup validates them.
fn credential_root(directory: &TempDir) -> PathBuf {
    let root = canonical_root(directory);
    emp_state::VaultStore::from_sources(None, &root.join("state/master.key"))
        .expect("initialize fixture credential store");
    root
}

fn response_timeout() -> Duration {
    test_timeout(Duration::from_secs(20))
}

fn complete_response(stream: &mut TcpStream) -> String {
    let deadline = Deadline::after(response_timeout());
    complete_response_from(&mut deadline.io(&*stream))
}

fn complete_response_from(stream: &mut impl Read) -> String {
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
    let deadline = Deadline::after(response_timeout());
    let mut stream = deadline.io(&*stream);
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8(response).expect("UTF-8 response")
}

fn request(server: &ServerHandle, target: &str, headers: &[&str]) -> String {
    request_before(
        server,
        target,
        headers,
        &Deadline::after(response_timeout()),
    )
}

fn request_before(
    server: &ServerHandle,
    target: &str,
    headers: &[&str],
    deadline: &Deadline,
) -> String {
    let stream = deadline.connect(server.local_addr()).expect("connect");
    let mut stream = deadline.io(stream);
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
    complete_response_from(&mut stream)
}

fn post(server: &ServerHandle, target: &str, body: &[u8], headers: &[&str]) -> String {
    post_before(
        server,
        target,
        body,
        headers,
        &Deadline::after(response_timeout()),
    )
}

fn post_before(
    server: &ServerHandle,
    target: &str,
    body: &[u8],
    headers: &[&str],
    deadline: &Deadline,
) -> String {
    let stream = deadline.connect(server.local_addr()).expect("connect");
    let mut stream = deadline.io(stream);
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
    complete_response_from(&mut stream)
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

struct ChunkedSse<R> {
    input: R,
    remaining: usize,
    ended: bool,
}

impl<R: BufRead> Read for ChunkedSse<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.ended || buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut length = String::new();
            self.input.read_line(&mut length)?;
            self.remaining = usize::from_str_radix(
                length.strip_suffix("\r\n").expect("HTTP chunk length line"),
                16,
            )
            .expect("HTTP chunk length");
            if self.remaining == 0 {
                let mut end = [0; 2];
                self.input.read_exact(&mut end)?;
                assert_eq!(&end, b"\r\n", "terminal HTTP chunk delimiter");
                self.ended = true;
                return Ok(0);
            }
        }
        let length = buffer.len().min(self.remaining);
        let count = self.input.read(&mut buffer[..length])?;
        assert!(count > 0, "HTTP chunk ended before its payload");
        self.remaining -= count;
        if self.remaining == 0 {
            let mut end = [0; 2];
            self.input.read_exact(&mut end)?;
            assert_eq!(&end, b"\r\n", "HTTP chunk delimiter");
        }
        Ok(count)
    }
}

fn read_sse_head<R: BufRead>(mut input: R) -> (String, BufReader<ChunkedSse<R>>) {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        input.read_line(&mut line).expect("read SSE headers");
        assert!(!line.is_empty(), "SSE response ended before headers");
        if line == "\r\n" {
            break;
        }
        head.push_str(&line);
    }
    assert!(head.contains("Transfer-Encoding: chunked\r\n"), "{head}");
    (
        head,
        BufReader::new(ChunkedSse {
            input,
            remaining: 0,
            ended: false,
        }),
    )
}

fn read_sse_frame(reader: &mut impl BufRead) -> String {
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

fn open_quota_events(server: &ServerHandle, session: &str) -> impl BufRead + use<> {
    let mut stream = TcpStream::connect(server.local_addr()).expect("connect SSE");
    stream
            .write_all(
                format!(
                    "GET /api/accounts/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{session}\r\nConnection: close\r\n\r\n",
                    server.local_addr().port()
                )
                .as_bytes(),
            )
            .expect("write SSE request");
    let (head, mut reader) = read_sse_head(BufReader::new(
        Deadline::after(test_timeout(Duration::from_secs(5))).io(stream),
    ));
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(head.contains("Content-Type: text/event-stream\r\n"));
    assert!(head.contains("Cache-Control: no-store\r\n"));
    assert!(head.contains("X-Accel-Buffering: no\r\n"));
    assert!(head.contains("X-Frame-Options: DENY\r\n"));
    assert!(head.contains("Content-Security-Policy: frame-ancestors 'none'\r\n"));
    assert!(head.contains("X-Content-Type-Options: nosniff\r\n"));
    assert_eq!(
        read_sse_frame(&mut reader),
        "event: quota-updated\ndata: {}\n"
    );
    assert_eq!(
        read_sse_frame(&mut reader),
        "event: integration-updated\ndata: {}\n"
    );
    let activity_frame = read_sse_frame(&mut reader);
    assert!(
        activity_frame.starts_with("event: activity-updated\ndata: "),
        "{activity_frame}"
    );
    let activity_body = activity_frame
        .strip_prefix("event: activity-updated\ndata: ")
        .expect("activity event prefix")
        .trim();
    let activity: Value = serde_json::from_str(activity_body).expect("activity snapshot");
    assert_eq!(activity["recent_for_seconds"], 60);
    assert_eq!(activity["routes"], json!([]));
    assert_eq!(
        activity
            .as_object()
            .expect("activity snapshot object")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "observed_at",
            "recent_for_seconds",
            "requests",
            "revision",
            "routes"
        ]
    );
    assert_eq!(
        read_sse_frame(&mut reader),
        "event: usage-updated\ndata: {}\n"
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

fn recorded_calls(server: &ServerHandle, session: &str) -> Value {
    let mut events = server.state.backend.management_events.subscribe().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async {
            events.next(Duration::ZERO).await.unwrap();
            let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
            loop {
                let wire = request_before(server, "/api/calls", &[session], &deadline);
                let report: Value =
                    serde_json::from_str(wire.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                if !report["records"].as_array().unwrap().is_empty() {
                    return deadline.accept(report, "call receipt persisted").unwrap();
                }
                assert!(
                    !deadline.is_expired(),
                    "call receipt not persisted: {report}"
                );
                loop {
                    let remaining = deadline.remaining();
                    if remaining.is_zero()
                        || events
                            .next(remaining)
                            .await
                            .expect("usage subscription closed")
                            .usage
                    {
                        break;
                    }
                }
            }
        })
}

fn websocket_event_before(
    socket: &mut emp_transport::ClientWebSocket,
    deadline: &Deadline,
) -> Value {
    loop {
        let remaining = deadline
            .accept(deadline.remaining(), "WebSocket response event")
            .unwrap();
        match socket.poll_receive_text_with_timeout(remaining).unwrap() {
            emp_transport::WebSocketPoll::Text(text) => {
                if let Ok(value @ Value::Object(_)) = serde_json::from_str(&text) {
                    return deadline.accept(value, "WebSocket response event").unwrap();
                }
            }
            emp_transport::WebSocketPoll::Ping(payload) => socket.send_pong(&payload).unwrap(),
            emp_transport::WebSocketPoll::Pending => {}
            other => panic!("WebSocket ended before response event: {other:?}"),
        }
    }
}

type CancellationProbe = (Deadline, mpsc::Sender<bool>);

fn report_cancellation(
    probe: &mpsc::Receiver<CancellationProbe>,
    observe: impl FnOnce(&Deadline) -> bool,
) {
    if let Ok((deadline, result)) = probe.recv() {
        let _ = result.send(observe(&deadline));
    }
}

fn wait_for_cancellation(probe: &mpsc::Sender<CancellationProbe>, deadline: &Deadline) -> bool {
    let (result, observed) = mpsc::channel();
    probe
        .send((*deadline, result))
        .expect("start cancellation probe");
    let closed = observed
        .recv_timeout(deadline.remaining())
        .expect("upstream observes cancellation");
    deadline.accept(closed, "upstream cancellation").unwrap()
}

fn upstream_closed_before(stream: &TcpStream, deadline: &Deadline) -> bool {
    match deadline.io(stream).peek(&mut [0]) {
        Ok(0) => true,
        Err(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
        ),
        Ok(_) => false,
    }
}

fn finish_upstream<T>(
    address: SocketAddr,
    worker: &mut Option<JoinHandle<std::io::Result<T>>>,
) -> std::io::Result<Option<T>> {
    let Some(worker) = worker.take() else {
        return Ok(None);
    };
    let _ = TcpStream::connect(address);
    worker
        .join()
        .map_err(|_| std::io::Error::other("fake upstream panicked"))?
        .map(Some)
}

pub(crate) struct OneShotUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<(String, BTreeMap<String, String>, Value)>,
    worker: Option<JoinHandle<()>>,
}

fn receive_upstream_request(stream: &mut TcpStream) -> (String, BTreeMap<String, String>, Value) {
    let raw = read_request_head(stream).expect("upstream request head");
    receive_upstream_request_from_head(stream, raw)
}

fn receive_upstream_request_from_head(
    stream: &TcpStream,
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
    let mut stream = Deadline::after(test_timeout(Duration::from_secs(5))).io(stream);
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
            emp_transport::MAX_EXPANDED_REQUEST_BYTES,
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
    pub(crate) fn start_repeated_wire(
        status: u16,
        content_type: &'static str,
        retry_after: Option<u64>,
        connections: usize,
        chunks: Vec<Vec<u8>>,
    ) -> Self {
        Self::serve(connections, move |stream| {
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
        })
    }

    pub(crate) fn serve(
        connections: usize,
        mut reply: impl FnMut(&mut TcpStream) + Send + 'static,
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
                reply(&mut stream);
            }
        });
        Self {
            address,
            observed,
            worker: Some(worker),
        }
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    pub(crate) fn observed(&self) -> (String, BTreeMap<String, String>, Value) {
        self.observed
            .recv_timeout(test_timeout(Duration::from_secs(5)))
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
            let result = worker.join();
            if !thread::panicking() {
                result.expect("join upstream");
            }
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
    let directory = emp_test_support::fixture_dir().expect("temporary directory");
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
        .test_config()
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

fn session_header(server: &ServerHandle) -> String {
    format!("X-EMP-Session: {}", server.session_token())
}

fn test_server() -> (TempDir, ServerHandle) {
    let directory = emp_test_support::fixture_dir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start server");
    (directory, server)
}
