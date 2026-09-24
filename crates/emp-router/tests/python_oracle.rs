use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_protocol::portable_responses::terminal_observation;
use emp_router::{ExternalRouter, ProjectionIds};
use emp_transport::{FailureClass, HttpClient, HttpClientPolicy};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
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

struct UpstreamServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl UpstreamServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
        listener
            .set_nonblocking(true)
            .expect("nonblocking upstream");
        let address = listener.local_addr().expect("upstream address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = {
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream
                                .set_nonblocking(true)
                                .expect("nonblocking accepted route stream");
                            stream
                                .set_nonblocking(false)
                                .expect("blocking accepted route stream");
                            let requests = Arc::clone(&requests);
                            thread::spawn(move || serve(stream, requests));
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

impl Drop for UpstreamServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join upstream");
        }
    }
}

fn serve(mut stream: TcpStream, requests: Arc<Mutex<Vec<RecordedRequest>>>) {
    let mut wire = Vec::new();
    let header_end = loop {
        if let Some(position) = wire.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        let mut buffer = [0_u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => wire.extend_from_slice(&buffer[..count]),
        }
    };
    let head = String::from_utf8(wire[..header_end].to_vec()).expect("ASCII request head");
    let mut lines = head.split("\r\n");
    let path = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
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
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => wire.extend_from_slice(&buffer[..count]),
        }
    }
    let body: Value = serde_json::from_slice(&wire[header_end..header_end + content_length])
        .expect("JSON request body");
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RecordedRequest {
            path: path.clone(),
            headers,
            body: body.clone(),
        });
    let (status, response) = if body.get("model").and_then(Value::as_str) == Some("fail") {
        (
            "503 Service Unavailable",
            json!({"error": {"message": "service unavailable"}}),
        )
    } else if body.get("model").and_then(Value::as_str) == Some("invalid") {
        ("200 OK", json!({"status": "cancelled", "output": []}))
    } else if path.ends_with("/chat/completions") {
        (
            "200 OK",
            json!({
                "id": "chat_upstream", "model": "upstream", "object": "chat.completion",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant", "reasoning_content": "think",
                        "content": "answer",
                        "tool_calls": [{
                            "id": "call_chat", "type": "function",
                            "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
            }),
        )
    } else if path.ends_with("/messages") {
        (
            "200 OK",
            json!({
                "id": "anthropic_upstream", "type": "message", "model": "upstream",
                "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "call_anthropic", "name": "lookup", "input": {"q": "x"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 3, "output_tokens": 2}
            }),
        )
    } else {
        (
            "200 OK",
            json!({
                "id": "responses_upstream", "object": "response", "model": "upstream",
                "status": "completed", "output_text": "answer",
                "output": [
                    {"id": "rs_upstream", "type": "reasoning", "content": [{"type": "reasoning_text", "text": "private chain"}]},
                    {"id": "item_upstream", "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer", "annotations": []}]},
                    {"id": "fc_upstream", "type": "function_call", "call_id": "call_responses", "name": "lookup", "arguments": "{\"q\":\"x\"}"}
                ],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
            }),
        )
    };
    let encoded = serde_json::to_vec(&response).expect("response JSON");
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        encoded.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&encoded);
    let _ = stream.flush();
}

fn provider(base_url: &str, protocol: Protocol) -> Map<String, Value> {
    let (protocol_name, auth_mode) = match protocol {
        Protocol::Auto => panic!("oracle fixture requires a concrete protocol"),
        Protocol::ChatCompletions => ("chat_completions", "api_key"),
        Protocol::AnthropicMessages => ("anthropic_messages", "anthropic_api_key"),
        Protocol::Responses => ("responses", "api_key"),
    };
    json!({
        "id": "demo", "name": "Demo", "base_url": base_url,
        "protocol": protocol_name, "auth_mode": auth_mode,
        "api_key": "test-key", "anthropic_version": "2023-06-01"
    })
    .as_object()
    .expect("provider object")
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
        json!({"id": "demo/model", "upstream_id": upstream_model})
            .as_object()
            .expect("model object")
            .clone(),
        protocol,
        dialect,
        "demo",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .expect("resolved route")
}

fn body() -> Value {
    json!({
        "model": "demo/model",
        "input": [{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
        "tools": [{
            "type": "function", "name": "lookup", "description": "Lookup",
            "parameters": {
                "type": "object", "properties": {"q": {"type": "string"}},
                "required": ["q"], "additionalProperties": false
            }
        }],
        "reasoning": {"effort": "low"},
        "max_output_tokens": 64,
        "stream": false
    })
}

fn projection_ids() -> ProjectionIds {
    ProjectionIds::new(
        "resp_normalized",
        "msg_normalized",
        "rs_normalized",
        "rs_late_normalized",
    )
}

fn python_oracle(python: &str, base_url: &str) -> Value {
    let script = r#"
import json, sys
from easy_multi_provider.router import anthropic_completion, chat_completion, forward_responses
from easy_multi_provider.protocol_projection import responses_terminal_observation
from easy_multi_provider.transport_failures import failure_from_exception

base = sys.argv[1]
body = {
    "model": "demo/model",
    "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}],
    "tools": [{"type": "function", "name": "lookup", "description": "Lookup", "parameters": {
        "type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"], "additionalProperties": False}}],
    "reasoning": {"effort": "low"}, "max_output_tokens": 64, "stream": False,
}
incoming = {"X-EMP-Request-ID": "0123456789abcdef"}

def normalize(value):
    ids = {}
    counts = {"resp_": 0, "msg_": 0, "rs_": 0}
    names = {"resp_": "resp_normalized", "msg_": "msg_normalized", "rs_": "rs_normalized"}
    def walk(item):
        if isinstance(item, dict):
            return {key: walk(value) for key, value in item.items()}
        if isinstance(item, list):
            return [walk(value) for value in item]
        if isinstance(item, str):
            for prefix in names:
                if item.startswith(prefix):
                    if item not in ids:
                        counts[prefix] += 1
                        ids[item] = names[prefix] if counts[prefix] == 1 else names[prefix][:-10] + str(counts[prefix])
                    return ids[item]
        return item
    return walk(value)

results = {}
for protocol, function, auth, wire_protocol in (
    ("chat", chat_completion, "api_key", "chat_completions"),
    ("anthropic", anthropic_completion, "anthropic_api_key", "anthropic_messages"),
    ("responses", forward_responses, "api_key", "responses"),
):
    provider = {"id": "demo", "base_url": base, "protocol": wire_protocol,
                "auth_mode": auth, "api_key": "test-key", "anthropic_version": "2023-06-01"}
    status, content_type, raw = function(provider, body, {}, incoming, upstream_model="upstream")
    payload = normalize(json.loads(raw))
    results[protocol] = {"status": status, "content_type": content_type,
                         "terminal_status": payload.get("status"),
                         "terminal": responses_terminal_observation(payload), "body": payload}

provider = {"id": "demo", "base_url": base, "protocol": "chat_completions", "auth_mode": "api_key", "api_key": "test-key"}
try:
    chat_completion(provider, body, {}, incoming, upstream_model="fail")
except Exception as exc:
    failure = failure_from_exception(exc)
    results["failure"] = {
        "native_type": type(exc).__name__, "status": failure.status,
        "error_class": failure.error_class,
        "failure_reason": failure.failure_reason,
        "retry_after_seconds": failure.retry_after_seconds,
        "terminal": "error", "retry": False,
    }
else:
    raise AssertionError("503 unexpectedly succeeded")

provider = {"id": "demo", "base_url": base, "protocol": "responses", "auth_mode": "api_key", "api_key": "test-key"}
try:
    forward_responses(provider, body, {}, incoming, upstream_model="invalid")
except Exception as exc:
    failure = failure_from_exception(exc)
    results["invalid_response"] = {
        "native_type": type(exc).__name__, "status": failure.status,
        "error_class": failure.error_class,
        "failure_reason": failure.failure_reason,
        "retry_after_seconds": failure.retry_after_seconds,
        "terminal": "error", "retry": False,
    }
else:
    raise AssertionError("invalid Responses body unexpectedly succeeded")
json.dump(results, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(base_url)
        .current_dir(root)
        .output()
        .expect("spawn Python Router oracle");
    assert!(
        output.status.success(),
        "Python Router oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python Router JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_complete_routes_match_live_python_and_socket_contract() {
    let server = UpstreamServer::start();
    let oracle = std::env::var("EMP_PYTHON_INTEROP")
        .ok()
        .map(|python| python_oracle(&python, &server.base_url()));

    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let incoming = BTreeMap::from([("X-EMP-Request-ID".to_owned(), "0123456789abcdef".to_owned())]);
    let chat = router
        .execute_complete(
            &route(&server.base_url(), Protocol::ChatCompletions, "upstream"),
            &body(),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect("Chat complete route");
    let anthropic = router
        .execute_complete(
            &route(&server.base_url(), Protocol::AnthropicMessages, "upstream"),
            &body(),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect("Anthropic complete route");
    let responses = router
        .execute_complete(
            &route(&server.base_url(), Protocol::Responses, "upstream"),
            &body(),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect("Responses complete route");
    let failure = router
        .execute_complete(
            &route(&server.base_url(), Protocol::ChatCompletions, "fail"),
            &body(),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect_err("503 route must fail");
    let invalid_response = router
        .execute_complete(
            &route(&server.base_url(), Protocol::Responses, "invalid"),
            &body(),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect_err("invalid Responses body must fail");
    let terminal = |value: &Value| {
        let observed = terminal_observation(value, true).expect("projected terminal");
        json!({
            "status": observed.status,
            "success": observed.success,
            "error_class": observed.error_class,
        })
    };
    let rust = json!({
        "chat": {"status": chat.status, "content_type": chat.content_type, "terminal_status": chat.body["status"], "terminal": terminal(&chat.body), "body": chat.body},
        "anthropic": {"status": anthropic.status, "content_type": anthropic.content_type, "terminal_status": anthropic.body["status"], "terminal": terminal(&anthropic.body), "body": anthropic.body},
        "responses": {"status": responses.status, "content_type": responses.content_type, "terminal_status": responses.body["status"], "terminal": terminal(&responses.body), "body": responses.body},
        "failure": {
            "status": failure.status(), "error_class": failure.error_class().as_str(),
            "failure_reason": failure.failure_reason(),
            "retry_after_seconds": failure.retry_after_seconds(),
            "terminal": "error", "retry": false,
        },
        "invalid_response": {
            "status": invalid_response.status(), "error_class": invalid_response.error_class().as_str(),
            "failure_reason": invalid_response.failure_reason(),
            "retry_after_seconds": invalid_response.retry_after_seconds(),
            "terminal": "error", "retry": false,
        }
    });
    assert_eq!(rust["chat"]["status"], 200);
    assert_eq!(rust["chat"]["body"]["output_text"], "answer");
    assert_eq!(rust["anthropic"]["body"]["output_text"], "answer");
    assert_eq!(rust["responses"]["body"]["output_text"], "answer");
    assert_eq!(
        rust["responses"]["body"]["output"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(failure.error_class(), FailureClass::Upstream5xx);
    assert_eq!(invalid_response.error_class(), FailureClass::ProtocolError);

    if let Some(mut oracle) = oracle {
        assert_eq!(oracle["failure"]["native_type"], "UpstreamHTTPError");
        oracle["failure"]
            .as_object_mut()
            .expect("failure object")
            .remove("native_type");
        assert_eq!(
            oracle["invalid_response"]["native_type"],
            "ExternalProtocolError"
        );
        oracle["invalid_response"]
            .as_object_mut()
            .expect("invalid response object")
            .remove("native_type");
        assert_eq!(rust, oracle);
    }

    let requests = server.requests();
    let rust_requests = &requests[requests.len() - 5..];
    assert_eq!(
        rust_requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        [
            "/v1/chat/completions",
            "/v1/messages",
            "/v1/responses",
            "/v1/chat/completions",
            "/v1/responses"
        ]
    );
    assert_eq!(
        rust_requests[0]
            .headers
            .get("authorization")
            .map(String::as_str),
        Some("Bearer test-key")
    );
    assert_eq!(
        rust_requests[1]
            .headers
            .get("x-api-key")
            .map(String::as_str),
        Some("test-key")
    );
    assert_eq!(
        rust_requests[1]
            .headers
            .get("anthropic-version")
            .map(String::as_str),
        Some("2023-06-01")
    );
    assert!(rust_requests.iter().all(|request| {
        request
            .headers
            .get("x-emp-request-id")
            .is_some_and(|value| value == "0123456789abcdef")
    }));
    if requests.len() == 10 {
        for index in 0..5 {
            assert_eq!(requests[index].path, requests[index + 5].path);
            assert_eq!(requests[index].body, requests[index + 5].body);
        }
    }
}
