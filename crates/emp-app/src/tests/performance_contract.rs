use super::{canonical_root, request, session_header};
use crate::lifecycle::ServerHandle;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
fn responses_endpoint_records_visible_generation_and_full_request_rates_separately() {
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
    let cookie = session_header(&server);
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
    assert_eq!(record["performance_schema"], 5);
    assert_eq!(record["output_tokens"], 120);
    assert_eq!(record["reasoning_tokens"], 90);
    assert!(record["ttft_ms"].as_u64().unwrap() >= 100);
    // Upstream deltas can coalesce in the socket under scheduler load; only
    // first-token and total pacing are deterministic EMP contracts.
    let duration_ms = record["duration_ms"].as_u64().unwrap();
    assert!(record["ttft_ms"].as_u64().unwrap() <= duration_ms);
    assert!(
        duration_ms >= 200,
        "full request duration was {duration_ms}ms"
    );
    let expected = 120_000.0 / record["duration_ms"].as_f64().unwrap();
    assert!((record["tokens_per_second"].as_f64().unwrap() - expected).abs() <= 0.01);
    assert!(record["first_content_ms"].as_u64().unwrap() >= 100);
    // A single coalesced batch has no measurable generation interval.
    match record["generation_ms"]
        .as_f64()
        .filter(|duration| *duration > 0.0)
    {
        Some(duration) => assert!(
            (record["generation_tokens_per_second"].as_f64().unwrap() - 30_000.0 / duration).abs()
                <= 0.01
        ),
        None => assert!(record["generation_tokens_per_second"].is_null()),
    }
    server.shutdown().expect("shutdown performance EMP");
}
