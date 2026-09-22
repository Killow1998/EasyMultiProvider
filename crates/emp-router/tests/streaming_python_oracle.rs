use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_router::{ExternalRouter, ProjectionIds, StreamResponseEvent};
use emp_transport::{HttpClient, HttpClientPolicy};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
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
    fixtures: Arc<Value>,
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
        let thread = {
            let fixtures = Arc::clone(&fixtures);
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
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
            fixtures,
            requests,
            shutdown,
            thread: Some(thread),
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
        assert!(self.fixtures.is_object());
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
        Protocol::Auto => panic!("oracle fixture requires a concrete protocol"),
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
        Protocol::Auto => panic!("oracle fixture requires a concrete protocol"),
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

async fn collect(router: &ExternalRouter<'_>, route: &ResolvedRoute) -> Vec<StreamResponseEvent> {
    let mut stream = router
        .open_stream(route, &body(), &BTreeMap::new(), &ids())
        .await
        .expect("open external stream");
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await.expect("stream event") {
        events.push(event);
    }
    assert!(stream.is_finished());
    events
}

async fn collect_failure(
    router: &ExternalRouter<'_>,
    route: &ResolvedRoute,
) -> (Vec<StreamResponseEvent>, emp_router::RouterError) {
    let mut stream = router
        .open_stream(route, &body(), &BTreeMap::new(), &ids())
        .await
        .expect("open failing external stream");
    let mut events = Vec::new();
    loop {
        match stream.next_event().await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => panic!("incomplete stream unexpectedly succeeded"),
            Err(error) => return (events, error),
        }
    }
}

fn normalize_ids(value: &mut Value) {
    fn walk(value: &mut Value, ids: &mut BTreeMap<String, String>, counts: &mut [usize; 3]) {
        match value {
            Value::Array(items) => {
                for item in items {
                    walk(item, ids, counts);
                }
            }
            Value::Object(items) => {
                for item in items.values_mut() {
                    walk(item, ids, counts);
                }
            }
            Value::String(text) => {
                for (index, prefix) in ["resp_", "msg_", "rs_"].iter().enumerate() {
                    if text.starts_with(prefix) {
                        let replacement = ids.entry(text.clone()).or_insert_with(|| {
                            counts[index] += 1;
                            format!("{prefix}normalized_{}", counts[index])
                        });
                        *text = replacement.clone();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(protocols) = value.as_object_mut() {
        for events in protocols.values_mut() {
            walk(events, &mut BTreeMap::new(), &mut [0, 0, 0]);
        }
    } else {
        walk(value, &mut BTreeMap::new(), &mut [0, 0, 0]);
    }
}

fn python_oracle(fixtures: &Value) -> Value {
    let python = std::env::var("EMP_PYTHON_INTEROP").expect("configured Python oracle");
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider import router
from easy_multi_provider.transport import sse_json_events

fixtures = json.load(sys.stdin)
body = {
    "model":"demo/model", "stream":True,
    "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}],
    "reasoning":{"effort":"low"}, "max_output_tokens":64,
}

class Upstream:
    status = 200
    def __init__(self, chunks, content_type="text/event-stream"):
        self.chunks = [chunk.encode() for chunk in chunks]
        self.headers = {"Content-Type":content_type,
                        "Content-Length":str(sum(map(len, self.chunks)))}
    def __iter__(self):
        return iter(self.chunks)
    def close(self):
        pass
    def finish(self):
        pass

def provider(protocol, auth):
    return {"id":"demo","base_url":"https://example.invalid/v1","protocol":protocol,
            "auth_mode":auth,"api_key":"test-key","anthropic_version":"2023-06-01"}

result = {}
cases = [
    ("chat", router.stream_chat_completion, provider("chat_completions", "api_key")),
    ("anthropic", router.stream_anthropic_completion, provider("anthropic_messages", "anthropic_api_key")),
]
for name, function, config in cases:
    with patch.object(router, "_request", return_value=Upstream(fixtures[name])):
        result[name] = list(sse_json_events(function(config, body, {}, {}, upstream_model="upstream")))
with patch.object(router, "_request", return_value=Upstream(fixtures["responses"])):
    result["responses"] = list(sse_json_events(router.forward_responses_stream(
        provider("responses", "api_key"), body, {}, {}, upstream_model="upstream")))
with patch.object(router, "_request", return_value=Upstream(fixtures["chat_ordinary"], "application/json")):
    result["chat_ordinary"] = list(sse_json_events(router.stream_chat_completion(
        provider("chat_completions", "api_key"), body, {}, {}, upstream_model="ordinary")))
with patch.object(router, "_request", return_value=Upstream(fixtures["chat_incomplete"])):
    incomplete = list(sse_json_events(router.stream_chat_completion(
        provider("chat_completions", "api_key"), body, {}, {}, upstream_model="incomplete")))
    error = incomplete[-1]["response"]["error"]
    result["chat_incomplete"] = {"status":error["status"], "error_class":error["error_class"]}
json.dump(result, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python streaming Router oracle");
    child
        .stdin
        .take()
        .expect("Python oracle stdin")
        .write_all(
            serde_json::to_string(fixtures)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write streaming fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python streaming Router oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python streaming oracle JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn external_streams_match_live_python_and_socket_contract() {
    let fixtures = stream_fixtures();
    let oracle = std::env::var("EMP_PYTHON_INTEROP")
        .ok()
        .map(|_| python_oracle(&fixtures));
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
    let ordinary = collect(
        &router,
        &route(&server.base_url(), Protocol::ChatCompletions, "ordinary"),
    )
    .await;
    let (_partial, incomplete) = collect_failure(
        &router,
        &route(&server.base_url(), Protocol::ChatCompletions, "incomplete"),
    )
    .await;
    let mut rust = json!({
        "chat": chat.into_iter().map(|event| event.body).collect::<Vec<_>>(),
        "anthropic": anthropic.into_iter().map(|event| event.body).collect::<Vec<_>>(),
        "responses": responses.into_iter().map(|event| event.body).collect::<Vec<_>>(),
        "chat_ordinary": ordinary.into_iter().map(|event| event.body).collect::<Vec<_>>(),
        "chat_incomplete": {
            "status": incomplete.status(),
            "error_class": incomplete.error_class().as_str(),
        },
    });
    normalize_ids(&mut rust);
    if let Some(mut oracle) = oracle {
        normalize_ids(&mut oracle);
        assert_eq!(rust, oracle);
    }
    assert_eq!(
        rust["chat"].as_array().unwrap().last().unwrap()["type"],
        "response.completed"
    );
    assert_eq!(
        rust["anthropic"].as_array().unwrap().last().unwrap()["type"],
        "response.completed"
    );
    assert_eq!(
        rust["responses"].as_array().unwrap().last().unwrap()["type"],
        "response.completed"
    );
    assert!(rust["responses"].as_array().unwrap().iter().all(|event| {
        !event["type"]
            .as_str()
            .unwrap_or_default()
            .contains("reasoning_text")
    }));

    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/responses",
            "/v1/chat/completions",
            "/v1/chat/completions"
        ]
    );
    assert!(requests.iter().all(|request| {
        request.body["stream"] == true
            && request.headers.get("accept").map(String::as_str) == Some("text/event-stream")
    }));
    assert_eq!(
        requests
            .iter()
            .map(|request| request.body["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["upstream", "upstream", "upstream", "ordinary", "incomplete"]
    );
    assert_eq!(requests[0].body["stream_options"]["include_usage"], true);
}
