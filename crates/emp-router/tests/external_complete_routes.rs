//! External complete-route behavior a Codex user observes when EMP forwards
//! a Responses request to a non-native provider: correct endpoint paths and
//! credentials per protocol, tool calls and reasoning kept out of the answer
//! text, and upstream errors surfaced with stable classes.

use emp_core::{Dialect, Protocol, ResolvedRoute, RouteSource};
use emp_protocol::portable_responses::terminal_observation;
use emp_router::{ExternalRouter, ProjectionIds};
use emp_transport::{FailureClass, HttpClient, HttpClientPolicy};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
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
        let worker = {
            let requests = Arc::clone(&requests);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                // model -> attempt count; the first "retry" model request is
                // dropped to prove the transport retries the POST once.
                let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
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
                            let attempts = Arc::clone(&attempts);
                            thread::spawn(move || {
                                serve(stream, requests, attempts);
                            });
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

impl Drop for UpstreamServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join upstream");
        }
    }
}

fn serve(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    attempts: Arc<Mutex<HashMap<String, usize>>>,
) {
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
    let body: Value =
        serde_json::from_slice(&wire[header_end..header_end + content_length]).expect("JSON body");
    requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(RecordedRequest {
            path: path.clone(),
            headers,
            body: body.clone(),
        });
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let attempt = {
        let mut attempts = attempts.lock().unwrap();
        let value = attempts.entry(model.to_owned()).or_default();
        let current = *value;
        *value += 1;
        current
    };
    let (status, response) = if model == "retry" && attempt == 0 {
        return; // drop the connection: the POST must be retried once
    } else if model == "fail" {
        (
            "503 Service Unavailable",
            json!({"error": {"message": "service unavailable"}}),
        )
    } else if model == "invalid" {
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
        Protocol::Auto => panic!("fixture requires a concrete protocol"),
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
        Protocol::Auto => panic!("fixture requires a concrete protocol"),
        Protocol::ChatCompletions => Dialect::ChatCompletions,
        Protocol::AnthropicMessages => Dialect::AnthropicMessages,
        Protocol::Responses => Dialect::PortableResponses,
    };
    ResolvedRoute::new(
        format!("demo/{upstream_model}"),
        upstream_model,
        RouteSource::ExplicitModel,
        provider(base_url, protocol),
        json!({"id": format!("demo/{upstream_model}"), "upstream_id": upstream_model})
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

fn body(upstream_model: &str) -> Value {
    json!({
        "model": format!("demo/{upstream_model}"),
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

fn terminal(value: &Value) -> Value {
    let observed = terminal_observation(value, true).expect("projected terminal");
    json!({
        "status": observed.status,
        "success": observed.success,
        "error_class": observed.error_class,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_protocol_reaches_its_own_endpoint_with_its_own_credentials() {
    let server = UpstreamServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let incoming = BTreeMap::from([("X-EMP-Request-ID".to_owned(), "0123456789abcdef".to_owned())]);

    let chat = router
        .execute_complete(
            &route(&server.base_url(), Protocol::ChatCompletions, "upstream"),
            &body("upstream"),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect("Chat complete route");
    let anthropic = router
        .execute_complete(
            &route(&server.base_url(), Protocol::AnthropicMessages, "upstream"),
            &body("upstream"),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect("Anthropic complete route");
    let responses = router
        .execute_complete(
            &route(&server.base_url(), Protocol::Responses, "upstream"),
            &body("upstream"),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect("Responses complete route");

    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.path.as_str())
            .collect::<Vec<_>>(),
        ["/v1/chat/completions", "/v1/messages", "/v1/responses"]
    );
    assert_eq!(
        requests[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-key")
    );
    assert_eq!(
        requests[1].headers.get("x-api-key").map(String::as_str),
        Some("test-key")
    );
    assert_eq!(
        requests[1]
            .headers
            .get("anthropic-version")
            .map(String::as_str),
        Some("2023-06-01")
    );
    assert!(requests.iter().all(|request| {
        request
            .headers
            .get("x-emp-request-id")
            .is_some_and(|value| value == "0123456789abcdef")
    }));

    for result in [&chat, &anthropic, &responses] {
        assert_eq!(result.status, 200);
        assert_eq!(result.content_type, "application/json");
        assert_eq!(result.body["output_text"], "answer");
        assert_eq!(terminal(&result.body)["success"], true);
    }
    // Tool calls and reasoning stay out of the answer text but remain visible
    // as output items: reasoning, message, function_call.
    assert_eq!(chat.body["output"].as_array().unwrap().len(), 3);
    assert_eq!(chat.body["output"][2]["type"], "function_call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_failures_map_to_stable_public_error_classes() {
    let server = UpstreamServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let incoming = BTreeMap::new();

    let failure = router
        .execute_complete(
            &route(&server.base_url(), Protocol::ChatCompletions, "fail"),
            &body("fail"),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect_err("503 route must fail");
    assert_eq!(failure.status(), 503);
    assert_eq!(failure.error_class(), FailureClass::Upstream5xx);

    let invalid_response = router
        .execute_complete(
            &route(&server.base_url(), Protocol::Responses, "invalid"),
            &body("invalid"),
            &incoming,
            &projection_ids(),
        )
        .await
        .expect_err("invalid Responses body must fail");
    assert_eq!(invalid_response.status(), 502);
    assert_eq!(invalid_response.error_class(), FailureClass::ProtocolError);
}

// The external router surfaces one dropped connection as a Network failure;
// the application layer (not the router) owns the retry policy. The native
// router's single retry is covered in native_http_complete.rs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_connection_is_surfaced_as_a_network_failure() {
    let server = UpstreamServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let error = router
        .execute_complete(
            &route(&server.base_url(), Protocol::Responses, "retry"),
            &body("retry"),
            &BTreeMap::new(),
            &projection_ids(),
        )
        .await
        .expect_err("a dropped connection is a transport failure");
    assert_eq!(error.status(), 503);
    assert_eq!(error.error_class(), FailureClass::Network);
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.body["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["retry"],
        "the router sends the POST once and leaves retries to the app"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_passthrough_preserves_cli_body_and_raw_response() {
    let server = UpstreamServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let router = ExternalRouter::new(&client);
    let request = json!({
        "model":"claude-cli-model",
        "stream":true,
        "system":[{"type":"text","text":"protocol metadata"}],
        "messages":[{"role":"user","content":[{"type":"text","text":"serialized transcript"}]}],
        "tools":[{"name":"StructuredOutput","input_schema":{"type":"object"}}],
        "output_config":{"effort":"high"}
    });
    let response = router
        .execute_anthropic_passthrough(
            &route(&server.base_url(), Protocol::AnthropicMessages, "upstream"),
            &request,
            &BTreeMap::new(),
            &BTreeMap::from([(
                "anthropic-beta".to_owned(),
                "interleaved-thinking".to_owned(),
            )]),
        )
        .await
        .expect("native Messages passthrough");

    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "application/json");
    let expected_response = json!({
        "id":"anthropic_upstream", "type":"message", "model":"upstream",
        "content":[
            {"type":"text","text":"answer"},
            {"type":"tool_use","id":"call_anthropic","name":"lookup","input":{"q":"x"}}
        ],
        "stop_reason":"tool_use", "usage":{"input_tokens":3,"output_tokens":2}
    });
    assert_eq!(
        response.body,
        serde_json::to_vec(&expected_response).unwrap()
    );

    let observed = server.requests();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].path, "/v1/messages");
    assert_eq!(observed[0].body["model"], "upstream");
    assert_eq!(observed[0].body["messages"], request["messages"]);
    assert_eq!(observed[0].body["system"], request["system"]);
    assert_eq!(observed[0].body["tools"], request["tools"]);
    assert_eq!(observed[0].body["output_config"], request["output_config"]);
    assert_eq!(observed[0].headers["x-api-key"], "test-key");
    assert_eq!(
        observed[0].headers["anthropic-beta"],
        "interleaved-thinking"
    );
    assert_eq!(observed[0].headers["accept"], "text/event-stream");
}
