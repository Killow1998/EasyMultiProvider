use super::{canonical_root, request, session_cookie_header};
use crate::lifecycle::ServerHandle;
use crate::services::observation::request_tokens_per_second;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PYTHON_TPS_ORACLE: &str = r#"
import json, os, sys
root = os.environ["EMP_PYTHON_ORACLE_ROOT"]
sys.path.insert(0, root)
import easy_multi_provider
assert easy_multi_provider.__version__ == "0.11.10", easy_multi_provider.__version__
from easy_multi_provider.performance import request_tokens_per_second
cases = json.load(sys.stdin)
json.dump([request_tokens_per_second(case["output_tokens"], case["duration_ms"]) for case in cases], sys.stdout)
"#;

#[test]
fn request_tps_matches_live_python_01110_reference() {
    let python = std::env::var("EMP_PYTHON_INTEROP")
        .expect("EMP_PYTHON_INTEROP must point at the current Python venv");
    let oracle = std::env::var("EMP_PYTHON_ORACLE_ROOT")
        .expect("EMP_PYTHON_ORACLE_ROOT must point at the current Python checkout");
    let cases = vec![
        json!({"output_tokens":120,"duration_ms":2000}),
        json!({"output_tokens":123,"duration_ms":4567}),
        json!({"output_tokens":1,"duration_ms":86_400_000}),
        json!({"output_tokens":0,"duration_ms":100}),
        json!({"output_tokens":true,"duration_ms":100}),
        json!({"output_tokens":10_000_001,"duration_ms":100}),
        json!({"output_tokens":120,"duration_ms":99}),
        json!({"output_tokens":120,"duration_ms":86_400_001}),
        json!({"output_tokens":120,"duration_ms":100_000_000}),
    ];
    let mut oracle_process = Command::new(python)
        .args(["-c", PYTHON_TPS_ORACLE])
        .env("EMP_PYTHON_ORACLE_ROOT", oracle)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start live Python TPS oracle");
    oracle_process
        .stdin
        .take()
        .expect("Python oracle stdin")
        .write_all(&serde_json::to_vec(&cases).expect("TPS cases JSON"))
        .expect("write TPS cases");
    let output = oracle_process
        .wait_with_output()
        .expect("wait for Python TPS oracle");
    assert!(
        output.status.success(),
        "Python TPS oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<Value> = serde_json::from_slice(&output.stdout).expect("Python TPS result");
    let actual = cases
        .iter()
        .map(|case| {
            request_tokens_per_second(&case["output_tokens"], &case["duration_ms"])
                .map_or(Value::Null, |rate| json!(rate))
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

struct TimedUsageUpstream {
    address: SocketAddr,
    worker: Option<JoinHandle<()>>,
}

impl TimedUsageUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind usage upstream");
        let address = listener.local_addr().expect("usage upstream address");
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept usage request");
            let raw =
                crate::http::request::read_request_head(&mut stream).expect("usage request head");
            let parsed = crate::http::request::parse_request(&raw.head).expect("usage request");
            let length = parsed
                .header("Content-Length")
                .and_then(|value| value.parse::<usize>().ok())
                .expect("usage request content length");
            let mut body = raw.body_prefix;
            while body.len() < length {
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).expect("read usage request body");
                assert_ne!(count, 0, "usage request body ended early");
                body.extend_from_slice(&buffer[..count]);
            }
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .expect("usage SSE response head");
            stream
                .write_all(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"perf\",\"status\":\"in_progress\",\"model\":\"upstream\"}}\n\n")
                .expect("usage created event");
            stream.flush().expect("flush usage created event");
            thread::sleep(Duration::from_millis(120));
            stream
                .write_all(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n")
                .expect("usage text event");
            stream.flush().expect("flush usage text event");
            thread::sleep(Duration::from_millis(120));
            stream
                .write_all(b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"!\"}\n\n")
                .expect("usage second text event");
            stream.flush().expect("flush usage second text event");
            thread::sleep(Duration::from_millis(120));
            stream
                .write_all(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"perf\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"upstream\",\"output\":[],\"usage\":{\"output_tokens\":120,\"output_tokens_details\":{\"reasoning_tokens\":90}}}}\n\n")
                .expect("usage completed event");
            stream.flush().expect("flush usage completed event");
        });
        Self {
            address,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}

impl Drop for TimedUsageUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join usage upstream");
        }
    }
}

fn post_response_stream(server: &ServerHandle, body: &[u8], cookie: &str) -> String {
    let mut stream = TcpStream::connect(server.local_addr()).expect("connect EMP responses");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set response timeout");
    write!(
        stream,
        "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nAuthorization: Bearer caller\r\n{cookie}\r\nConnection: close\r\n\r\n",
        server.local_addr().port(),
        body.len()
    )
    .expect("write responses head");
    stream.write_all(body).expect("write responses body");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read streamed response");
    String::from_utf8(response).expect("stream response UTF-8")
}

#[test]
fn responses_endpoint_records_schema3_full_request_tps_and_preserves_stream_timings() {
    let upstream = TimedUsageUpstream::start();
    let directory = tempfile::tempdir().expect("performance temp directory");
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "providers":[{
                "id":"forward",
                "name":"Forward",
                "base_url":upstream.base_url(),
                "protocol":"responses",
                "auth_mode":"forward"
            }],
            "models":[]
        }))
        .expect("encode performance config"),
    )
    .expect("write performance config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        native_auth_path,
    )
    .expect("start performance EMP");
    let cookie = session_cookie_header(&server);
    let body = serde_json::to_vec(&json!({
        "model":"gpt-6-luna",
        "input":"hello",
        "stream":true
    }))
    .expect("encode response request");
    let response = post_response_stream(&server, &body, &cookie);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("response.output_text.delta"));
    assert!(response.contains("response.completed"));

    let diagnostics = request(&server, "/api/diagnostics", &[&cookie]);
    assert!(
        diagnostics.starts_with("HTTP/1.1 200 OK\r\n"),
        "{diagnostics}"
    );
    let body = diagnostics
        .split_once("\r\n\r\n")
        .expect("diagnostics response separator")
        .1;
    let snapshot: Value = serde_json::from_str(body).expect("diagnostics JSON");
    let record = snapshot["records"]
        .as_array()
        .expect("diagnostic records")
        .iter()
        .find(|record| record["model_id"] == "gpt-6-luna")
        .expect("record for the Responses request");
    assert_eq!(record["performance_schema"], 3);
    assert_eq!(record["output_tokens"], 120);
    assert!(record["ttft_ms"].as_u64().unwrap() >= 100);
    // Upstream deltas can coalesce in the socket under scheduler load; only
    // first-token and total pacing are deterministic EMP contracts.
    let generation_ms = record["generation_ms"].as_u64().unwrap();
    let duration_ms = record["duration_ms"].as_u64().unwrap();
    assert!(
        generation_ms <= duration_ms,
        "generation {generation_ms}ms exceeded duration {duration_ms}ms"
    );
    assert!(
        duration_ms >= 200,
        "full request duration was {duration_ms}ms"
    );
    let expected = request_tokens_per_second(&record["output_tokens"], &record["duration_ms"])
        .expect("measurable full-request TPS");
    assert_eq!(record["tokens_per_second"], json!(expected));
    server.shutdown().expect("shutdown performance EMP");
}
