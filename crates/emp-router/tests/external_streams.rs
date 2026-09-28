//! External streaming behavior users see in Codex: answers arrive as
//! Responses events, provider reasoning is never echoed into the answer
//! text, non-stream replies are re-framed as a single completed event, and a
//! truncated stream fails with a stable error instead of a silent success.

use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::{ExternalRouter, ProjectionIds};
use emp_transport::{FailureClass, HttpClient, HttpClientPolicy};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
struct RecordedRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct StreamServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StreamServer {
    fn start(fixtures: Value) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind stream upstream");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("stream upstream address");
        let fixtures = Arc::new(fixtures);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = {
            let fixtures = Arc::clone(&fixtures);
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream
                                .set_nonblocking(true)
                                .expect("nonblocking accepted stream route");
                            stream
                                .set_nonblocking(false)
                                .expect("blocking accepted stream route");
                            let fixtures = Arc::clone(&fixtures);
                            let requests = Arc::clone(&requests);
                            thread::spawn(move || serve(stream, &fixtures, requests));
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
            shutdown,
            thread: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}/v1", self.address.ip(), self.address.port())
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join stream upstream");
        }
    }
}

fn serve(mut stream: TcpStream, fixtures: &Value, requests: Arc<Mutex<Vec<RecordedRequest>>>) {
    let Some((path, headers, body)) = read_request(&mut stream) else {
        return;
    };
    let upstream_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RecordedRequest {
            path: path.clone(),
            headers,
            body,
        });
    let key = if path.ends_with("/chat/completions") && upstream_model == "ordinary" {
        "chat_ordinary"
    } else if path.ends_with("/chat/completions") && upstream_model == "incomplete" {
        "chat_incomplete"
    } else if path.ends_with("/chat/completions") {
        "chat"
    } else if path.ends_with("/messages") {
        "anthropic"
    } else {
        "responses"
    };
    let chunks = fixtures[key].as_array().expect("stream fixture chunks");
    let length = chunks
        .iter()
        .map(|chunk| chunk.as_str().expect("stream fixture text").len())
        .sum::<usize>();
    let content_type = if key == "chat_ordinary" {
        "application/json"
    } else {
        "text/event-stream"
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    for chunk in chunks {
        if stream
            .write_all(chunk.as_str().expect("stream fixture text").as_bytes())
            .is_err()
            || stream.flush().is_err()
        {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn read_request(stream: &mut TcpStream) -> Option<(String, BTreeMap<String, String>, Value)> {
    let mut wire = Vec::new();
    let header_end = loop {
        if let Some(position) = wire.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        wire.extend_from_slice(&buffer[..count]);
    };
    let head = String::from_utf8(wire[..header_end].to_vec()).ok()?;
    let mut lines = head.split("\r\n");
    let path = lines
        .next()?
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut headers = BTreeMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while wire.len() < header_end + content_length {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        wire.extend_from_slice(&buffer[..count]);
    }
    let body = serde_json::from_slice(&wire[header_end..header_end + content_length]).ok()?;
    Some((path, headers, body))
}

fn frame(value: Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(&value).expect("fixture JSON")
    )
}

fn split_wire(wire: String) -> Vec<Value> {
    // Deliver the SSE wire in awkward chunk sizes so no event depends on
    // arriving inside one TCP chunk.
    let bytes = wire.into_bytes();
    let mut result = Vec::new();
    let mut start = 0;
    for width in [1, 7, 3, 19, 5, 31] {
        if start >= bytes.len() {
            break;
        }
        let end = (start + width).min(bytes.len());
        result.push(Value::String(
            String::from_utf8(bytes[start..end].to_vec()).expect("ASCII fixture split"),
        ));
        start = end;
    }
    if start < bytes.len() {
        result.push(Value::String(
            String::from_utf8(bytes[start..].to_vec()).expect("ASCII fixture tail"),
        ));
    }
    result
}

fn stream_fixtures() -> Value {
    let chat = [
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{"reasoning_content":"think "},"finish_reason":null}]}),
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    ]
    .into_iter()
    .map(frame)
    .collect::<String>()
        + "data: [DONE]\n\n";
    let anthropic = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(frame)
    .collect::<String>()
        + "data: [DONE]\n\n";
    let chat_ordinary = serde_json::to_string(&json!({
        "id":"chat_ordinary","object":"chat.completion","model":"ordinary",
        "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
    }))
    .expect("ordinary Chat JSON");
    let chat_incomplete = frame(json!({
        "id":"chat_incomplete","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]
    }));
    let response = json!({
        "id":"responses_upstream","object":"response","status":"completed",
        "model":"upstream","output_text":"answer",
        "output":[
            {"id":"rs_private","type":"reasoning","status":"completed","summary":[],"content":[{"type":"reasoning_text","text":"private chain"}]},
            {"id":"msg_visible","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}
        ],
        "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
    });
    let responses = [
        json!({"type":"response.created","response":{"id":"responses_upstream","object":"response","status":"in_progress","model":"upstream","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"rs_private","type":"reasoning","status":"in_progress","summary":[],"content":[]}}),
        json!({"type":"response.reasoning_text.delta","item_id":"rs_private","output_index":0,"content_index":0,"delta":"private chain"}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_visible","type":"message","status":"in_progress","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","item_id":"msg_visible","output_index":1,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_visible","output_index":1,"content_index":0,"delta":"answer"}),
        json!({"type":"response.output_text.done","item_id":"msg_visible","output_index":1,"content_index":0,"text":"answer"}),
        json!({"type":"response.content_part.done","item_id":"msg_visible","output_index":1,"content_index":0,"part":{"type":"output_text","text":"answer","annotations":[]}}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"id":"msg_visible","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}}),
        json!({"type":"response.completed","response":response}),
    ]
    .into_iter()
    .map(frame)
    .collect::<String>()
        + "data: [DONE]\n\n";
    json!({
        "chat": split_wire(chat),
        "chat_ordinary": split_wire(chat_ordinary),
        "chat_incomplete": split_wire(chat_incomplete),
        "anthropic": split_wire(anthropic),
        "responses": split_wire(responses),
    })
}

fn provider(base_url: &str, protocol: Protocol) -> Map<String, Value> {
    let (wire_protocol, auth_mode) = match protocol {
        Protocol::Auto => panic!("fixture requires a concrete protocol"),
        Protocol::ChatCompletions => ("chat_completions", "api_key"),
        Protocol::AnthropicMessages => ("anthropic_messages", "anthropic_api_key"),
        Protocol::Responses => ("responses", "api_key"),
    };
    json!({
        "id":"demo", "base_url":base_url, "protocol":wire_protocol,
        "auth_mode":auth_mode, "api_key":"test-key", "anthropic_version":"2023-06-01"
    })
    .as_object()
    .expect("provider")
    .clone()
}

fn route(base_url: &str, protocol: Protocol, upstream_model: &str) -> ResolvedRoute {
    let dialect = match protocol {
        Protocol::Auto => panic!("fixture requires a concrete protocol"),
        Protocol::ChatCompletions => Dialect::ChatCompletions,
        Protocol::AnthropicMessages => Dialect::AnthropicMessages,
        Protocol::Responses => Dialect::PortableResponses,
    };
    ResolvedRoute::new(
        "demo/model",
        upstream_model,
        RouteSource::ExplicitModel,
        provider(base_url, protocol),
        json!({"id":"demo/model","upstream_id":upstream_model})
            .as_object()
            .expect("model")
            .clone(),
        protocol,
        dialect,
        "demo",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .expect("stream route")
}

fn body() -> Value {
    json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}],
        "reasoning":{"effort":"low"}, "max_output_tokens":64
    })
}

fn ids() -> ProjectionIds {
    ProjectionIds::new("resp_rust", "msg_rust", "rs_rust", "rs_late_rust")
}

async fn collect(router: &ExternalRouter<'_>, route: &ResolvedRoute) -> Vec<Value> {
    let mut stream = router
        .open_stream(route, &body(), &BTreeMap::new(), &ids())
        .await
        .expect("open external stream");
    let mut bodies = Vec::new();
    while let Some(event) = stream.next_event().await.expect("stream event") {
        bodies.push(event.body);
    }
    assert!(stream.is_finished());
    bodies
}

async fn collect_failure(
    router: &ExternalRouter<'_>,
    route: &ResolvedRoute,
) -> emp_router::RouterError {
    let mut stream = router
        .open_stream(route, &body(), &BTreeMap::new(), &ids())
        .await
        .expect("open failing external stream");
    let mut saw_partial = false;
    loop {
        match stream.next_event().await {
            Ok(Some(_event)) => saw_partial = true,
            Ok(None) => panic!("incomplete stream unexpectedly succeeded"),
            Err(error) => {
                assert!(saw_partial, "the answer delta must arrive before the error");
                return error;
            }
        }
    }
}

fn final_event(bodies: &[Value]) -> &Value {
    bodies.last().expect("stream produced events")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn every_stream_protocol_ends_with_one_completed_response() {
    let fixtures = stream_fixtures();
    let server = StreamServer::start(fixtures);
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let chat = collect(
        &router,
        &route(&server.base_url(), Protocol::ChatCompletions, "upstream"),
    )
    .await;
    let anthropic = collect(
        &router,
        &route(&server.base_url(), Protocol::AnthropicMessages, "upstream"),
    )
    .await;
    let responses = collect(
        &router,
        &route(&server.base_url(), Protocol::Responses, "upstream"),
    )
    .await;
    for bodies in [&chat, &anthropic, &responses] {
        assert_eq!(
            final_event(bodies)["type"],
            "response.completed",
            "the projected stream always terminates with a completed event"
        );
    }
    // Provider reasoning is visible as reasoning events but never echoed into
    // the answer text: the answer text only carries the content deltas.
    for bodies in [&chat, &anthropic] {
        assert!(
            bodies
                .iter()
                .any(|event| event["type"] == "response.output_text.delta"),
            "the answer text still streams to the client"
        );
        let answer: String = bodies
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect();
        assert_eq!(answer, "answer");
    }
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/v1/chat/completions", "/v1/messages", "/v1/responses"]
    );
    assert!(requests.iter().all(|request| {
        request.body["stream"] == true
            && request.headers.get("accept").map(String::as_str) == Some("text/event-stream")
    }));
    assert_eq!(
        requests[0].body["stream_options"]["include_usage"], true,
        "usage is requested so the completed event can carry token counts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_plain_json_reply_is_reframed_as_the_standard_response_event_sequence() {
    let fixtures = stream_fixtures();
    let server = StreamServer::start(fixtures);
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let ordinary = collect(
        &router,
        &route(&server.base_url(), Protocol::ChatCompletions, "ordinary"),
    )
    .await;
    let types: Vec<&str> = ordinary
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect();
    assert_eq!(
        types,
        [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.completed",
        ],
        "a plain JSON reply is re-framed as the standard Responses event sequence"
    );
    assert_eq!(final_event(&ordinary)["response"]["output_text"], "answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_truncated_stream_fails_instead_of_succeeding_silently() {
    let fixtures = stream_fixtures();
    let server = StreamServer::start(fixtures);
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let error = collect_failure(
        &router,
        &route(&server.base_url(), Protocol::ChatCompletions, "incomplete"),
    )
    .await;
    assert_eq!(error.error_class(), FailureClass::StreamIncomplete);
    assert_eq!(error.status(), 502);
}
