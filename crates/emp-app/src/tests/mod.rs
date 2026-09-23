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
use crate::services::quota::QuotaSampleCounts;
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
mod catalog_api_contract;
mod config_api_contract;
mod conversation_switch_contract;
mod native_api_contract;
mod performance_contract;
mod quota_workspace_contract;
mod websocket_capacity_contract;

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

    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
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
            buckets = {"codex": {
                "limitId": "codex",
                "primary": {"usedPercent": used, "windowDurationMins": 300, "resetsAt": 123},
                "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 456},
            }}
            if started == "imported-rotated":
                buckets["free"] = {"primary": {"usedPercent": 12, "windowDurationMins": 43200, "resetsAt": 789}}
            result = {
                "rateLimitsByLimitId": buckets,
                "rateLimitResetCredits": {"availableCount": 2, "credits": [{"id": "opaque-reset-id", "status": "available", "expiresAt": 1000, "title": "Full reset"}]},
            }
            print(json.dumps({"id": request["id"], "result": result}), flush=True)
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
        .configuration
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
        .configuration
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

    let unauthorized = post(&server, "/api/accounts/%40native/quota", b"{}", &[]);
    assert!(
        unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{unauthorized}"
    );
    let unknown = post(&server, "/api/accounts/missing/quota", b"{}", &[&cookie]);
    assert!(
        unknown.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{unknown}"
    );
    let unknown: Value = serde_json::from_str(
        unknown
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("unknown account error");
    assert_eq!(
        unknown,
        json!({"error":{"code":"quota_error","message":"unknown account: missing"}})
    );

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
        refreshed["account"]["quota"]["rate_limits"]["primary"]["windowDurationMins"],
        300
    );
    assert_eq!(
        refreshed["account"]["quota"]["rate_limits"]["secondary"]["windowDurationMins"],
        10080
    );
    assert_eq!(
        refreshed["account"]["quota"]["credits"]["reset_credits"]["available_count"],
        2
    );
    assert!(!refreshed.to_string().contains("opaque-reset-id"));
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
    assert_eq!(
        reset["account"]["quota"]["credits"]["reset_credits"]["credits"][0]["title"],
        "Full reset"
    );
    assert!(!reset.to_string().contains("opaque-reset-id"));

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
    assert_eq!(imported["account"]["quota"]["plan_type"], "free");
    assert_eq!(
        imported["account"]["quota"]["rate_limits"]["secondary"]["windowDurationMins"],
        10080
    );
    assert_eq!(
        imported["account"]["quota"]["rate_limits_by_limit_id"]["free"]["primary"]["windowDurationMins"],
        43200
    );
    let persisted_auth = server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&imported_auth)
        .expect("read rotated imported auth");
    assert_eq!(
        persisted_auth["tokens"]["access_token"], "imported-rotated",
        "a rotation completed before the first 401 must be reused by the retry"
    );

    let imported_reset = post(
        &server,
        "/api/accounts/egg/quota-reset",
        &reset_body,
        &[&cookie],
    );
    assert!(
        imported_reset.starts_with("HTTP/1.1 200 OK\r\n"),
        "{imported_reset}"
    );
    let imported_reset: Value = serde_json::from_str(
        imported_reset
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("imported reset response");
    assert_eq!(imported_reset["outcome"], "reset");
    assert_eq!(imported_reset["refresh_error"], Value::Null);
    assert_eq!(imported_reset["account"]["quota"]["plan_type"], "free");
    assert_eq!(
        imported_reset["account"]["quota"]["rate_limits"]["secondary"]["windowDurationMins"],
        10080
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
            .configuration
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

#[cfg(unix)]
#[test]
fn quota_rate_limit_http_error_is_safe() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state").join("accounts");
    let auth_file = account_root.join("rate-limited").join("auth.json.enc");
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "account_store_path": account_root,
            "accounts": [{
                "id": "rate-limited",
                "name": "Rate limited",
                "prefix": "rate-limited",
                "auth_file": auth_file,
            }]
        }))
        .expect("encode config"),
    )
    .expect("write config");

    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("fake Codex directory");
    let executable = executable_root.path().join("codex");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id": request["id"], "result": {}}), flush=True)
    elif method == "account/read":
        print(json.dumps({"id": request["id"], "result": {"account": {"email": "xian@example.com", "planType": "pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        print(json.dumps({"id": request["id"], "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 429 Too Many Requests; content-type=text/plain; body=private-token"}}), flush=True)
"#,
    )
    .expect("write fake Codex");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("make fake Codex executable");

    let native_auth = root.join("auth.json");
    std::fs::write(&native_auth, b"{}").expect("write native auth");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        executable.to_str().expect("UTF-8 executable path"),
        native_auth,
    )
    .expect("start quota server");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_file,
            &json!({
                "tokens": {
                    "access_token": "rate-limited",
                    "account_id": "workspace-rate-limited"
                }
            }),
        )
        .expect("write rate-limited auth");

    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/api/accounts/rate-limited/quota",
        b"{}",
        &[&cookie],
    );
    assert!(
        response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{response}"
    );
    let body: Value = serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("rate-limit error response");
    assert_eq!(
        body,
        json!({"error":{"code":"quota_rate_limited","message":"Codex quota queries are rate limited (429); try again later"}})
    );
    assert!(!response.contains("private-token"));
    assert!(!response.contains("example.invalid"));
    server.shutdown().expect("shutdown");
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
            host: Some("127.0.0.1".to_owned()),
            port: Some(0),
            open_browser: false,
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
fn idle_accept_worker_wakes_for_shutdown_after_serving_a_request() {
    let (_directory, server) = test_server();
    let response = request(&server, "/healthz", &[]);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));

    thread::sleep(Duration::from_millis(25));
    let started = Instant::now();
    server.shutdown().expect("idle listener shutdown");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "idle blocking accept did not wake promptly for shutdown"
    );
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
    std::fs::create_dir_all(session_path.parent().expect("state directory")).expect("create state");
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
    std::fs::write(&auth, br#"{"tokens":{"access_token":"native-secret"}}"#).expect("write auth");
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
    }))
    .unwrap();
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
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind compaction upstream");
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
    let stored = target
        .state
        .backend
        .configuration
        .config
        .lock()
        .unwrap()
        .clone();
    let provider = stored["providers"].as_array().unwrap()[0].clone();
    assert_eq!(
        provider_api_key(&provider, &target.state.backend.configuration.vault),
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
    let stored = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .unwrap()
        .clone();
    let auth_path = PathBuf::from(stored["accounts"][0]["auth_file"].as_str().unwrap());
    assert!(auth_path.is_file());
    assert!(auth_path.parent().unwrap().join("config.toml").is_file());
    assert_eq!(
        server
            .state
            .backend
            .configuration
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
        server.state.backend.configuration.config.lock().unwrap()["accounts"]
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

#[test]
fn integration_api_applies_and_shutdown_restores_only_owned_codex_fields() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("emp-config.json");
    // Python rejects applying an empty model picker; this success scenario needs
    // the same visible model fixture as tests.test_server._integration_test_config.
    std::fs::write(&config, serde_json::to_vec(&json!({
        "providers":[{"id":"external","base_url":"https://example.invalid/v1","protocol":"responses"}],
        "models":[{"id":"external/model-a","provider":"external","upstream_id":"model-a","enabled":true}]
    })).unwrap()).unwrap();
    let codex_config = root.join("config.toml");
    std::fs::write(
        &codex_config,
        b"# keep\nopenai_base_url = \"native\"\n[features]\nweb_search = true\n",
    )
    .unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let before = request(
        &server,
        "/api/integration",
        &[&session_cookie_header(&server)],
    );
    assert!(before.contains("\"state\":\"native\""), "{before}");
    let enabled = post(
        &server,
        "/api/integration/enable",
        br#"{"confirm_reload":true}"#,
        &[&session_cookie_header(&server)],
    );
    assert!(enabled.starts_with("HTTP/1.1 200 OK\r\n"), "{enabled}");
    assert!(enabled.contains("\"state\":\"emp_applied\""));
    let applied = std::fs::read_to_string(&codex_config).unwrap();
    assert!(applied.contains(&format!(
        "openai_base_url = \"http://127.0.0.1:{}/v1\"",
        server.local_addr().port()
    )));
    assert!(applied.contains("model_catalog_json"));
    assert!(applied.contains("[features]\nweb_search = true"));
    server.shutdown().unwrap();
    let restored = std::fs::read_to_string(&codex_config).unwrap();
    assert!(restored.contains("openai_base_url = \"native\""));
    assert!(!restored.contains("model_catalog_json"));
    assert!(restored.contains("[features]\nweb_search = true"));
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

fn assert_stream_protocol(protocol: &str, auth_mode: &str, expected_path: &str, events: &[Value]) {
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
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
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
