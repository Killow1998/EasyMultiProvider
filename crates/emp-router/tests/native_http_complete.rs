//! Native (Codex) buffered HTTP behavior: zstd-framed requests to the
//! Responses endpoint, credential refresh on 401, reasoning-effort fallback,
//! context-length and rate-limit surfacing, and the single network retry.

use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::native_http::{NativeHttpError, NativeRouter};
use emp_router::native_request::{NativeAuth, request_headers};
use emp_transport::{HttpClient, HttpClientPolicy, decode_content};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
struct RecordedRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

struct NativeUpstream {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl NativeUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind native upstream");
        listener
            .set_nonblocking(true)
            .expect("nonblocking upstream");
        let address = listener.local_addr().expect("upstream address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream
                                .set_nonblocking(true)
                                .expect("nonblocking accepted native stream");
                            stream
                                .set_nonblocking(false)
                                .expect("blocking accepted native stream");
                            let requests = Arc::clone(&requests);
                            let attempts = Arc::clone(&attempts);
                            thread::spawn(move || serve(stream, requests, attempts));
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for NativeUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join upstream");
        }
    }
}

fn receive(mut stream: &TcpStream) -> RecordedRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request timeout");
    let mut wire = Vec::new();
    let header_end = loop {
        if let Some(position) = wire.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).expect("read request head");
        assert!(count > 0, "request ended before headers");
        wire.extend_from_slice(&chunk[..count]);
    };
    let head = String::from_utf8(wire[..header_end].to_vec()).expect("ASCII request head");
    assert!(
        head.starts_with("POST /v1/responses HTTP/1.1\r\n"),
        "{head}"
    );
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let length = headers["content-length"].parse::<usize>().unwrap();
    while wire.len() < header_end + length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).expect("read request body");
        assert!(count > 0, "request ended before body");
        wire.extend_from_slice(&chunk[..count]);
    }
    let encoding = headers
        .get("content-encoding")
        .map(String::as_str)
        .unwrap_or("");
    let decoded = decode_content(
        wire[header_end..header_end + length].to_vec(),
        encoding,
        4 * 1024 * 1024,
        None,
    )
    .expect("decode upstream body");
    RecordedRequest {
        headers,
        body: serde_json::from_slice(&decoded).expect("upstream request JSON"),
    }
}

fn serve(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    attempts: Arc<Mutex<HashMap<String, usize>>>,
) {
    let request = receive(&stream);
    let case = request.body["_case"]
        .as_str()
        .expect("fixture case")
        .to_owned();
    requests.lock().unwrap().push(request.clone());
    let attempt = {
        let mut attempts = attempts.lock().unwrap();
        let value = attempts.entry(case.clone()).or_default();
        let current = *value;
        *value += 1;
        current
    };
    if case == "network_once" && attempt == 0 {
        return; // drop the connection without responding
    }
    let (status, body) = match case.as_str() {
        "account_refresh" if attempt == 0 => (
            401,
            json!({"error":{"message":"expired selected credential"}}),
        ),
        "account_refresh_failed" | "forward_401" => {
            (401, json!({"error":{"message":"unauthorized"}}))
        }
        "reasoning_fallback" if request.body.get("reasoning_effort").is_some() => (
            400,
            json!({"error":{"message":"unknown field reasoning_effort"}}),
        ),
        "rate_limit" => (429, json!({"error":{"message":"rate limited"}})),
        "gateway_timeout" => (504, json!({"error":{"message":"gateway timeout"}})),
        "context" => (
            400,
            json!({"error":{"code":"context_length_exceeded","message":"maximum context length exceeded"}}),
        ),
        "large_error" => (
            400,
            // Noise stays inside the 4 KiB per-text evidence cap so the
            // trailing context marker is still visible to the classifier.
            json!({"message":format!("{} context length exceeded","x".repeat(2000))}),
        ),
        "buried_error" => (
            400,
            json!({"message":format!("{} context length exceeded","x".repeat(5000))}),
        ),
        _ => (
            200,
            json!({
                "id":"resp_fixture", "object":"response", "status":"completed",
                "model":"upstream", "output":[], "future":{"opaque":[1,true,"x"]}
            }),
        ),
    };
    let encoded = serde_json::to_vec(&body).unwrap();
    let retry = if case == "rate_limit" {
        "Retry-After: 1.2\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "HTTP/1.1 {status} reason\r\nContent-Type: application/json\r\nContent-Length: {}\r\nOpenAI-Model: upstream\r\nX-Codex-Turn-State: fixture-turn\r\nX-Models-Etag: upstream-etag\r\n{retry}Connection: close\r\n\r\n",
        encoded.len()
    )
    .expect("write response head");
    stream.write_all(&encoded).expect("write response body");
}

fn route(base_url: &str, account: bool) -> ResolvedRoute {
    let provider = json!({
        "id":"native", "base_url":base_url, "protocol":"responses",
        "auth_mode":if account {"account"} else {"forward"},
        "account":if account {json!({"id":"fixture-account"})} else {Value::Null}
    });
    ResolvedRoute::new(
        "requested",
        "upstream",
        RouteSource::ExplicitModel,
        provider.as_object().unwrap().clone(),
        json!({"id":"requested","upstream_id":"upstream"})
            .as_object()
            .unwrap()
            .clone(),
        Protocol::Responses,
        Dialect::CodexNative,
        "native",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .unwrap()
}

fn lower_headers(headers: BTreeMap<String, String>) -> Map<String, Value> {
    headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), Value::String(value)))
        .collect()
}

fn incoming() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".to_owned(), "Bearer caller".to_owned()),
        ("chatgpt-account-id".to_owned(), "caller-owner".to_owned()),
        ("thread-id".to_owned(), "thread-fixture".to_owned()),
        (
            "x-openai-subagent".to_owned(),
            "subagent-fixture".to_owned(),
        ),
        ("X-EMP-Request-ID".to_owned(), "0123456789abcdef".to_owned()),
    ])
}

fn resolved_headers(
    incoming: &BTreeMap<String, String>,
    account: bool,
    rotated: bool,
) -> Result<BTreeMap<String, String>, NativeHttpError> {
    let selected = BTreeMap::from([
        (
            "Authorization".to_owned(),
            format!("Bearer {}", if rotated { "rotated" } else { "selected" }),
        ),
        (
            "chatgpt-account-id".to_owned(),
            format!("{}-owner", if rotated { "rotated" } else { "selected" }),
        ),
    ]);
    request_headers(
        if account {
            NativeAuth::Account(&selected)
        } else {
            NativeAuth::Forward
        },
        incoming,
        false,
    )
    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
}

async fn run(
    upstream: &NativeUpstream,
    case: &str,
    account: bool,
    extra: Option<serde_json::Map<String, Value>>,
) -> (Value, u64) {
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let mut body = json!({"model":"requested","input":"hello","stream":false,"_case":case})
        .as_object()
        .unwrap()
        .clone();
    if let Some(extra) = extra {
        for (key, value) in extra {
            body.insert(key, value);
        }
    }
    let mut refreshes = 0_u64;
    let result = router
        .execute_complete(
            &route(&upstream.base_url(), account),
            &body,
            false,
            true,
            |refresh| {
                if refresh {
                    refreshes += 1;
                }
                resolved_headers(&incoming(), account, refreshes > 0)
            },
        )
        .await;
    let value = match result {
        Ok(result) => json!({
            "status":result.status,"content_type":result.content_type,
            "body":serde_json::from_slice::<Value>(&result.body).unwrap(),
            "headers":lower_headers(result.headers),"refreshes":refreshes
        }),
        Err(error) => json!({
            "status":error.status,"content_type":"application/json","body":error.body,
            "headers":lower_headers(error.headers),"refreshes":refreshes
        }),
    };
    (value, refreshes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_requests_are_zstd_framed_and_carry_selected_credentials() {
    let upstream = NativeUpstream::start();
    let (success, refreshes) = run(&upstream, "success", false, None).await;
    assert_eq!(refreshes, 0);
    assert_eq!(success["status"], 200);
    assert_eq!(success["content_type"], "application/json");
    assert_eq!(success["body"]["status"], "completed");
    // Opaque future fields survive the passthrough untouched.
    assert_eq!(success["body"]["future"], json!({"opaque":[1,true,"x"]}));
    // Selected upstream metadata headers pass to the client; secrets do not.
    // Upstream model headers are aliased back to the requested model.
    let response_headers = success["headers"].as_object().expect("headers object");
    assert_eq!(response_headers["openai-model"], "requested");
    assert_eq!(response_headers["x-codex-turn-state"], "fixture-turn");
    assert_eq!(response_headers["x-models-etag"], "upstream-etag");
    assert!(!response_headers.contains_key("authorization"));

    let request = &upstream.requests()[0];
    assert_eq!(
        request.headers.get("content-encoding").map(String::as_str),
        Some("zstd")
    );
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer caller")
    );
    assert_eq!(
        request.headers.get("thread-id").map(String::as_str),
        Some("thread-fixture")
    );
    assert_eq!(
        request.body["model"], "upstream",
        "the requested model is routed to the upstream id"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_account_credential_is_refreshed_exactly_once() {
    let upstream = NativeUpstream::start();
    let (result, refreshes) = run(&upstream, "account_refresh", true, None).await;
    assert_eq!(refreshes, 1);
    assert_eq!(result["status"], 200);
    let request = &upstream.requests()[0];
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer selected")
    );

    let (failure, refreshes) = run(&upstream, "account_refresh_failed", true, None).await;
    assert_eq!(
        refreshes, 1,
        "a failing refresh still consumed the single retry"
    );
    assert_eq!(failure["status"], 401);
    assert_eq!(failure["body"]["error"]["type"], "auth");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_reasoning_effort_is_dropped_and_the_request_retried() {
    let upstream = NativeUpstream::start();
    let (result, _refreshes) = run(
        &upstream,
        "reasoning_fallback",
        false,
        Some(
            json!({"reasoning_effort": "low"})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .await;
    assert_eq!(result["status"], 200);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body.get("reasoning_effort").is_some());
    assert!(requests[1].body.get("reasoning_effort").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limits_and_gateway_timeouts_surface_retry_information() {
    let upstream = NativeUpstream::start();
    let (rate, _refreshes) = run(&upstream, "rate_limit", false, None).await;
    assert_eq!(rate["status"], 429);
    assert_eq!(rate["body"]["error"]["code"], "rate_limit_exceeded");
    assert_eq!(rate["body"]["error"]["retry_after_seconds"], 2);
    assert_eq!(rate["headers"]["retry-after"], "2");

    let (timeout, _refreshes) = run(&upstream, "gateway_timeout", false, None).await;
    assert_eq!(timeout["status"], 504);
    assert_eq!(timeout["body"]["error"]["type"], "upstream_504");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_length_errors_become_413_even_inside_noise() {
    let upstream = NativeUpstream::start();
    let (context, _refreshes) = run(&upstream, "context", false, None).await;
    assert_eq!(context["status"], 413);

    // Noise within the error-evidence budget does not hide the marker; noise
    // beyond the 4 KiB per-text cap does, and the 400 passes through as-is.
    let (large, _refreshes) = run(&upstream, "large_error", false, None).await;
    assert_eq!(large["status"], 413);

    let (buried, _refreshes) = run(&upstream, "buried_error", false, None).await;
    assert_eq!(buried["status"], 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_connection_is_retried_once_then_the_request_succeeds() {
    let upstream = NativeUpstream::start();
    let (result, _refreshes) = run(&upstream, "network_once", false, None).await;
    assert_eq!(result["status"], 200);
    assert_eq!(upstream.requests().len(), 2, "exactly one network retry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_401_without_a_refreshable_account_fails_immediately() {
    let upstream = NativeUpstream::start();
    let (result, refreshes) = run(&upstream, "forward_401", false, None).await;
    assert_eq!(
        refreshes, 0,
        "forward mode has no account credential to refresh"
    );
    assert_eq!(result["status"], 401);
    assert_eq!(upstream.requests().len(), 1);
}
