//! Native (Codex) streaming behavior: events flow through with model
//! headers rewritten, bad JSON frames are skipped, collaboration tool calls
//! survive in plaintext mode, and 401/429 openings surface as errors.

use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::native_http::{NativeHttpError, NativeRouter};
use emp_router::native_request::{NativeAuth, request_headers};
use emp_transport::{HttpClient, HttpClientPolicy};
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
    let decoded = emp_transport::decode_content(
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
    if case == "stream_account_refresh" && attempt == 0 {
        let body = br#"{"error":{"message":"expired selected credential"}}"#;
        write!(stream,"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
        stream.write_all(body).unwrap();
        return;
    }
    if case == "stream_truncated" {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream
            .write_all(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n")
            .unwrap();
        stream.flush().unwrap();
        return; // EOF without a terminal event
    }
    if case == "stream_rate" {
        let body = br#"{"error":{"message":"rate limited"}}"#;
        write!(stream,"HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 1.2\r\nConnection: close\r\n\r\n",body.len()).unwrap();
        stream.write_all(body).unwrap();
        return;
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nOpenAI-Model: upstream\r\nX-Codex-Turn-State: fixture-turn\r\nConnection: close\r\n\r\n").unwrap();
    let frame = |value: &[u8]| {
        let mut frame = Vec::from(&b"data: "[..]);
        frame.extend_from_slice(value);
        frame.extend_from_slice(b"\n\n");
        frame
    };
    let mut frames = vec![
        frame(br#"{"type":"response.created","response":{"id":"resp_stream","status":"in_progress","headers":{"openai-model":"upstream"}}}"#),
        frame(b"{bad-json}"),
    ];
    if case == "stream_collaboration" {
        frames.push(frame(br#"{"type":"response.output_item.done","item":{"type":"function_call","namespace":"emp_collaboration","name":"spawn_agent","arguments":"{}"}}"#));
    }
    frames.push(frame(
        br#"{"type":"response.output_text.delta","delta":"hello"}"#,
    ));
    frames.push(frame(br#"{"type":"response.completed","response":{"id":"resp_stream","object":"response","status":"completed","model":"upstream","output":[],"headers":{"x-openai-model":"upstream"},"future":true}}"#));
    for frame in frames {
        // Deliver in 11-byte pieces so no event depends on one TCP chunk.
        for chunk in frame.chunks(11) {
            stream.write_all(chunk).unwrap();
            stream.flush().unwrap();
        }
    }
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

fn ids() -> emp_router::ProjectionIds {
    emp_router::ProjectionIds::new(
        "resp_fallback",
        "msg_fallback",
        "rs_fallback",
        "rs_late_fallback",
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_streams_deliver_created_delta_and_completed_events() {
    let upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let body = json!({"model":"requested","input":"hello","stream":true,"_case":"stream_success"})
        .as_object()
        .unwrap()
        .clone();
    let mut stream = router
        .open_stream(
            &route(&upstream.base_url(), false),
            &body,
            false,
            &ids(),
            |refresh| {
                assert!(!refresh);
                request_headers(NativeAuth::Forward, &incoming(), true)
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
            },
        )
        .await
        .expect("native stream opens");

    // Upstream model headers are aliased back to the requested model.
    let headers = lower_headers(stream.headers.clone());
    assert_eq!(headers["openai-model"], "requested");
    assert_eq!(headers["x-codex-turn-state"], "fixture-turn");

    let mut names = Vec::new();
    let mut completed = Value::Null;
    while let Some(event) = stream.next_event().await.expect("stream event") {
        let is_completed = event.event == "response.completed";
        if is_completed {
            completed = event.body.clone();
        }
        names.push(event.event);
        if is_completed {
            break;
        }
    }
    assert_eq!(
        names,
        [
            "response.created",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    // Model headers inside event payloads are aliased to the requested model.
    assert_eq!(
        completed["response"]["headers"]["x-openai-model"],
        "requested"
    );
    let request = &upstream.requests()[0];
    assert_eq!(
        request.headers.get("content-encoding").map(String::as_str),
        Some("zstd")
    );
    assert_eq!(request.body["model"], "upstream");
    assert_eq!(request.body["stream"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_upstream_frames_are_skipped_and_the_stream_still_completes() {
    let upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let body = json!({"model":"requested","input":"hello","stream":true,"_case":"stream_success"})
        .as_object()
        .unwrap()
        .clone();
    let mut stream = router
        .open_stream(
            &route(&upstream.base_url(), false),
            &body,
            false,
            &ids(),
            |refresh| {
                assert!(!refresh);
                request_headers(NativeAuth::Forward, &incoming(), true)
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
            },
        )
        .await
        .expect("native stream opens");

    // The upstream interleaves a `{bad-json}` frame; it must neither hang
    // the stream nor abort it, and the valid events around it still arrive.
    let mut names = Vec::new();
    while let Some(event) = stream.next_event().await.expect("stream event") {
        let is_completed = event.event == "response.completed";
        names.push(event.event);
        if is_completed {
            break;
        }
    }
    assert_eq!(
        names,
        [
            "response.created",
            "response.output_text.delta",
            "response.completed"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_stream_credential_fails_the_opening_without_a_refresh() {
    // Stream openings do not retry: a 401 surfaces as a stable auth error so
    // the caller can pick another account before any bytes reach the client.
    let upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let body =
        json!({"model":"requested","input":"hello","stream":true,"_case":"stream_account_refresh"})
            .as_object()
            .unwrap()
            .clone();
    let selected = BTreeMap::from([
        ("Authorization".to_owned(), "Bearer selected".to_owned()),
        ("chatgpt-account-id".to_owned(), "selected-owner".to_owned()),
    ]);
    let error = match router
        .open_stream(
            &route(&upstream.base_url(), true),
            &body,
            false,
            &ids(),
            |refresh| {
                assert!(!refresh, "stream openings never ask for a refresh");
                request_headers(NativeAuth::Account(&selected), &incoming(), true)
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
            },
        )
        .await
    {
        Ok(_stream) => panic!("a 401 opening is an error, not a stream"),
        Err(error) => error,
    };
    assert_eq!(error.status, 401);
    assert_eq!(error.body["error"]["code"], "auth_rejected");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer selected")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limited_streams_surface_retry_information_before_opening() {
    let upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let body = json!({"model":"requested","input":"hello","stream":true,"_case":"stream_rate"})
        .as_object()
        .unwrap()
        .clone();
    let error = match router
        .open_stream(
            &route(&upstream.base_url(), false),
            &body,
            false,
            &ids(),
            |refresh| {
                assert!(!refresh);
                request_headers(NativeAuth::Forward, &incoming(), true)
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
            },
        )
        .await
    {
        Ok(_stream) => panic!("a 429 opening is an error, not a stream"),
        Err(error) => error,
    };
    assert_eq!(error.status, 429);
    assert_eq!(error.body["error"]["code"], "rate_limit_exceeded");
    assert_eq!(error.body["error"]["retry_after_seconds"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_tool_calls_pass_through_in_plaintext_mode() {
    let upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let body = json!({
        "model":"requested","input":"hello","stream":true,"_case":"stream_collaboration",
        "tools":[{"type":"namespace","name":"collaboration","tools":[{
            "type":"function","name":"spawn_agent","parameters":{"type":"object","properties":{
                "message":{"type":"string","encrypted":true}}}}]
        }]
    })
    .as_object()
    .unwrap()
    .clone();
    let mut stream = router
        .open_stream(
            &route(&upstream.base_url(), false),
            &body,
            true,
            &ids(),
            |refresh| {
                assert!(!refresh);
                request_headers(NativeAuth::Forward, &incoming(), true)
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
            },
        )
        .await
        .expect("collaboration stream opens");
    let mut saw_function_call = false;
    while let Some(event) = stream.next_event().await.unwrap() {
        if event.event == "response.output_item.done" {
            saw_function_call = true;
            break;
        }
    }
    assert!(
        saw_function_call,
        "the collaboration tool call reaches the client"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_ends_without_a_terminal_event_fails_as_incomplete() {
    let upstream = NativeUpstream::start();
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let router = NativeRouter::new(&client);
    let body =
        json!({"model":"requested","input":"hello","stream":true,"_case":"stream_truncated"})
            .as_object()
            .unwrap()
            .clone();
    let mut stream = router
        .open_stream(
            &route(&upstream.base_url(), false),
            &body,
            false,
            &ids(),
            |refresh| {
                assert!(!refresh);
                request_headers(NativeAuth::Forward, &incoming(), true)
                    .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
            },
        )
        .await
        .expect("truncated stream still opens");
    let mut saw_delta = false;
    let error = loop {
        match stream.next_event().await {
            Ok(Some(event)) => saw_delta |= event.event == "response.output_text.delta",
            Ok(None) => panic!("a truncated stream must not report success"),
            Err(error) => break error,
        }
    };
    assert!(saw_delta, "the delta before the truncation still arrives");
    assert_eq!(error.status(), 502);
    assert_eq!(
        error.error_class(),
        emp_transport::FailureClass::StreamIncomplete
    );
}
