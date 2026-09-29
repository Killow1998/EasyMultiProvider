//! Installed-Claude end-to-end checks use a fake Anthropic CPA endpoint.
use super::*;

fn claude_server(base_url: &str) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "id":"demo", "name":"Demo", "base_url":base_url,
                "protocol":"anthropic_messages", "auth_mode":"api_key",
                "api_key":"upstream-secret", "execution_backend":"claude_cli"
            }],
            "models": [{
                "id":"demo/model", "provider":"demo", "upstream_id":"sonnet",
                "enabled":true
            }]
        }))
        .expect("encode config"),
    )
    .expect("write config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start Claude CLI server");
    (directory, server)
}

fn claude_summary_server(base_url: &str) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "id":"demo", "name":"Demo", "base_url":base_url,
                "protocol":"auto", "auth_mode":"api_key",
                "api_key":"upstream-secret", "execution_backend":"claude_cli"
            }],
            "models": [
                {
                    "id":"demo/long", "provider":"demo", "upstream_id":"long-context-model",
                    "enabled":true, "context_window":100000, "output_limit":256,
                    "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}
                },
                {
                    "id":"demo/short", "provider":"demo", "upstream_id":"sonnet",
                    "enabled":true, "context_window":5000, "output_limit":64,
                    "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}
                }
            ]
        }))
        .expect("encode config"),
    )
    .expect("write config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start short-destination server");
    (directory, server)
}

fn structured_messages_sse(proposal: &Value) -> Vec<u8> {
    structured_messages_sse_with_usage(
        proposal,
        json!({"input_tokens":12}),
        json!({"output_tokens":1}),
    )
}

fn structured_messages_sse_with_usage(
    proposal: &Value,
    input_usage: Value,
    output_usage: Value,
) -> Vec<u8> {
    let partial = serde_json::to_string(proposal).expect("proposal JSON");
    let events = [
        json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":"sonnet","content":[],"stop_reason":null,"stop_sequence":null,"usage":input_usage}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_fixture","name":"StructuredOutput","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":partial}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":output_usage}),
        json!({"type":"message_stop"}),
    ];
    let mut output = Vec::new();
    for event in events {
        let name = event["type"].as_str().expect("event type");
        output.extend_from_slice(b"event: ");
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(b"\ndata: ");
        output.extend_from_slice(&serde_json::to_vec(&event).expect("event JSON"));
        output.extend_from_slice(b"\n\n");
    }
    output
}

fn user_transcript(messages: &Value) -> Value {
    let text = messages["messages"]
        .as_array()
        .expect("Messages body")
        .iter()
        .find(|message| message["role"] == "user")
        .and_then(|message| match message.get("content") {
            Some(Value::String(text)) => Some(text.as_str()),
            Some(Value::Array(parts)) => parts
                .iter()
                .find(|part| part["type"] == "text")
                .and_then(|part| part["text"].as_str()),
            _ => None,
        })
        .expect("serialized transcript");
    serde_json::from_str(text).expect("Responses transcript JSON")
}

struct SummaryRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
    transcript: Value,
    final_turn: bool,
}

struct SummaryUpstream {
    address: std::net::SocketAddr,
    requests: mpsc::Receiver<SummaryRequest>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SummaryUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind summary CPA");
        listener
            .set_nonblocking(true)
            .expect("nonblocking summary CPA");
        let address = listener.local_addr().expect("summary CPA address");
        let (sender, requests) = mpsc::channel();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = std::sync::Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !worker_stopped.load(std::sync::atomic::Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                stream
                    .set_nonblocking(false)
                    .expect("normalize accepted summary CPA socket");
                let (path, headers, body) = receive_upstream_request(&mut stream);
                let transcript = user_transcript(&body);
                let final_turn = transcript
                    .to_string()
                    .contains("active destination request");
                if sender
                    .send(SummaryRequest {
                        path,
                        headers,
                        body,
                        transcript,
                        final_turn,
                    })
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(200));
                let answer = if final_turn {
                    "short destination answer"
                } else {
                    "portable checkpoint"
                };
                let response_body = structured_messages_sse(&json!({
                    "answer":answer,
                    "tool_calls":[]
                }));
                let response_head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                if stream.write_all(response_head.as_bytes()).is_err()
                    || stream.write_all(&response_body).is_err()
                {
                    break;
                }
                if final_turn {
                    break;
                }
            }
        });
        Self {
            address,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn requests_through_final(&self) -> Vec<SummaryRequest> {
        let deadline = Instant::now() + Duration::from_secs(9);
        let mut requests = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "short-destination inference did not arrive"
            );
            let request = self
                .requests
                .recv_timeout(remaining)
                .expect("bounded summary CPA request");
            let final_turn = request.final_turn;
            requests.push(request);
            if final_turn {
                return requests;
            }
        }
    }
}

impl Drop for SummaryUpstream {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join summary CPA");
        }
    }
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_forwards_one_messages_request_and_only_projects_structured_output() {
    // The actual CLI process runs with a temporary HOME and dummy auth token.
    // Every CPA response is local synthetic SSE; no upstream credentials exist.
    assert!(
        emp_codex::installed_cli::resolve_claude_cli().is_some(),
        "trusted Claude Code CLI must be on PATH"
    );

    let marker_directory = tempfile::tempdir().expect("tool execution marker directory");
    let marker = marker_directory.path().join("cli-tool-executed");
    let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse_with_usage(
        &json!({
            "answer":"",
            "tool_calls":[{
                "type":"function_call", "name":"Bash", "namespace":"",
                "arguments":{"command":format!("touch {}", marker.display())}, "input":""
            }]
        }),
        json!({
            "input_tokens":12,
            "cache_read_input_tokens":7000,
            "cache_creation_input_tokens":3000,
            "cache_creation":{"ephemeral_1h_input_tokens":2500}
        }),
        json!({"output_tokens":3}),
    )]);
    let (_directory, server) = claude_server(&upstream.base_url());
    let history = json!({
        "model":"demo/model", "stream":false,
        "reasoning":{"effort":"low"},
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"history user"}]},
            {"type":"function_call","call_id":"call_previous","name":"Bash","arguments":"{\"command\":\"printf previous\"}"},
            {"type":"function_call_output","call_id":"call_previous","output":"exact prior tool result: newline\nquote \" café 🧪"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"follow-up"}]}
        ],
        "tools":[{"type":"function","name":"Bash","description":"test-only proposal","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}]
    });
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&history).expect("history JSON"),
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value = serde_json::from_str(response.split_once("\r\n\r\n").expect("HTTP body").1)
        .expect("Responses JSON");
    assert_eq!(result["output"][0]["type"], "function_call");
    assert_eq!(result["output"][0]["name"], "Bash");
    assert_eq!(
        serde_json::from_str::<Value>(result["output"][0]["arguments"].as_str().unwrap()).unwrap()
            ["command"],
        format!("touch {}", marker.display())
    );
    assert_eq!(result["usage"]["input_tokens"], 10012);
    assert_eq!(
        result["usage"]["input_tokens_details"]["cached_tokens"],
        7000
    );
    assert_eq!(
        result["usage"]["input_tokens_details"]["cache_creation_tokens"],
        3000
    );
    assert_eq!(
        result["usage"]["input_tokens_details"]["cache_creation_1h_tokens"],
        2500
    );
    assert_eq!(result["usage"]["output_tokens"], 3);
    assert_eq!(result["usage"]["total_tokens"], 10015);
    assert!(
        !marker.exists(),
        "EMP and Claude Code must not execute Codex tools"
    );
    let (path, headers, upstream_body) = upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(upstream_body["model"], "sonnet");
    assert!(headers.contains_key("anthropic-version"), "{headers:?}");
    assert_eq!(upstream_body["stream"], true);
    assert_eq!(upstream_body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(upstream_body["tools"][0]["name"], "StructuredOutput");
    let transcript = user_transcript(&upstream_body);
    assert_eq!(transcript["model"], "demo/model");
    assert_eq!(transcript["input"][2]["call_id"], "call_previous");
    assert_eq!(
        transcript["input"][2]["output"],
        "exact prior tool result: newline\nquote \" café 🧪"
    );
    assert_eq!(transcript["tools"], history["tools"]);

    server.shutdown().expect("shutdown HTTP tool server");

    let sse_upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
        "answer":"buffered SSE answer", "tool_calls":[]
    }))]);
    let (_sse_directory, sse_server) = claude_server(&sse_upstream.base_url());
    let sse = post_stream(
        &sse_server,
        "/v1/responses",
        &serde_json::to_vec(&json!({"model":"demo/model","stream":true,"input":"stream request"}))
            .expect("SSE request"),
        &[&session_header(&sse_server)],
    );
    assert!(sse.starts_with("HTTP/1.1 200 OK\r\n"), "{sse}");
    assert!(sse.contains("event: response.output_text.delta\n"), "{sse}");
    assert!(sse.contains("buffered SSE answer"), "{sse}");
    assert!(sse.contains("event: response.completed\n"), "{sse}");
    let (path, _, body) = sse_upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(body["model"], "sonnet");
    assert_eq!(body["stream"], true);
    sse_server.shutdown().expect("shutdown SSE server");

    let ws_upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
        "answer":"websocket answer", "tool_calls":[]
    }))]);
    let (_ws_directory, ws_server) = claude_server(&ws_upstream.base_url());
    let url = format!("ws://{}/v1/responses", ws_server.local_addr());
    let headers = BTreeMap::from([(
        "x-emp-session".to_owned(),
        session_header(&ws_server)
            .trim_start_matches("X-EMP-Session: ")
            .to_owned(),
    )]);
    let mut socket =
        emp_transport::ClientWebSocket::connect(&url, &headers, Duration::from_secs(5))
            .expect("connect websocket");
    socket
        .send_json(
            &json!({"type":"response.create","model":"demo/model","input":"websocket request"}),
        )
        .expect("send websocket turn");
    let mut completed = false;
    for _ in 0..12 {
        let event = socket
            .receive_json()
            .expect("websocket event")
            .expect("websocket remains open");
        if event["type"] == "response.output_text.delta" {
            assert_eq!(event["delta"], "websocket answer");
        }
        if event["type"] == "response.completed" {
            completed = true;
            break;
        }
        assert_ne!(
            event["type"], "error",
            "unexpected websocket error: {event}"
        );
    }
    assert!(completed, "websocket response did not complete");
    let (path, _, body) = ws_upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(body["model"], "sonnet");
    assert_eq!(body["stream"], true);
    socket.close();
    ws_server.shutdown().expect("shutdown websocket server");

    let compact_upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
        "answer":"portable explicit checkpoint", "tool_calls":[]
    }))]);
    let (_compact_directory, compact_server) = claude_server(&compact_upstream.base_url());
    let compact = post(
        &compact_server,
        "/v1/responses/compact",
        &serde_json::to_vec(&json!({
            "model":"demo/model",
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"explicit compact request"}]}]
        }))
        .expect("explicit compact request JSON"),
        &[&session_header(&compact_server)],
    );
    assert!(compact.starts_with("HTTP/1.1 200 OK\r\n"), "{compact}");
    let compact_result: Value = serde_json::from_str(compact.split_once("\r\n\r\n").unwrap().1)
        .expect("explicit compaction response JSON");
    assert_eq!(compact_result["output"][0]["type"], "compaction");
    assert!(
        compact_result["output"][0]["encrypted_content"]
            .as_str()
            .is_some_and(|value| value.starts_with("emp1:"))
    );
    let (path, headers, upstream_body) = compact_upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(upstream_body["model"], "sonnet");
    assert!(
        user_transcript(&upstream_body)
            .to_string()
            .contains("CONTEXT CHECKPOINT COMPACTION")
    );
    compact_server
        .shutdown()
        .expect("shutdown explicit compact server");
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_websocket_preserves_router_error_class_and_retry_after() {
    assert!(
        emp_codex::installed_cli::resolve_claude_cli().is_some(),
        "trusted Claude Code CLI must be on PATH"
    );

    let error_body = serde_json::to_vec(&json!({
        "error":{"message":"private synthetic CPA error"}
    }))
    .expect("synthetic error JSON");
    let upstream = OneShotUpstream::start_wire(429, "application/json", Some(2), vec![error_body]);
    let (_directory, server) = claude_server(&upstream.base_url());
    let url = format!("ws://{}/v1/responses", server.local_addr());
    let headers = BTreeMap::from([(
        "x-emp-session".to_owned(),
        session_header(&server)
            .trim_start_matches("X-EMP-Session: ")
            .to_owned(),
    )]);
    let mut socket =
        emp_transport::ClientWebSocket::connect(&url, &headers, Duration::from_secs(5))
            .expect("connect websocket");
    socket
        .send_json(&json!({
            "type":"response.create",
            "model":"demo/model",
            "input":"websocket rate limit request"
        }))
        .expect("send websocket turn");
    let event = (0..4)
        .map(|_| {
            socket
                .receive_json()
                .expect("websocket error event")
                .expect("websocket remains open after a turn error")
        })
        .find(|event| event["type"] != "codex.response.metadata")
        .expect("websocket error event after metadata");
    assert_eq!(event["type"], "error");
    assert_eq!(event["status"], 429);
    assert_eq!(event["error"]["code"], "rate_limit_exceeded");
    assert_eq!(event["error"]["error_class"], "rate_limit");
    assert_eq!(event["error"]["retry_after_seconds"], 2);
    assert!(
        !event.to_string().contains("private synthetic CPA error"),
        "upstream error text must stay private"
    );
    let (path, _, request) = upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(request["model"], "sonnet");

    socket.close();
    server.shutdown().expect("shutdown websocket error server");
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_handles_bounded_long_history_compaction_at_short_destination() {
    assert!(
        emp_codex::installed_cli::resolve_claude_cli().is_some(),
        "trusted Claude Code CLI must be on PATH"
    );

    let upstream = SummaryUpstream::start();
    let (_directory, server) = claude_summary_server(&upstream.base_url());
    let mut input = Vec::new();
    for turn in 0..4 {
        input.push(json!({
            "type":"message", "role":"user",
            "content":[{"type":"input_text","text":format!("long-context user turn {turn}: {}", "u".repeat(2400))}]
        }));
        input.push(json!({
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":format!("long-context assistant turn {turn}: {}", "a".repeat(2400))}]
        }));
    }
    input.push(json!({
        "type":"message", "role":"user",
        "content":[{"type":"input_text","text":"active destination request"}]
    }));
    let body = serde_json::to_vec(&json!({
        "model":"demo/short",
        "stream":false,
        "max_output_tokens":64,
        "input":input
    }))
    .expect("long-context request JSON");

    let started = Instant::now();
    let response = post(&server, "/v1/responses", &body, &[&session_header(&server)]);
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "bounded destination summary and completion"
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1)
        .expect("destination Responses body");
    assert_eq!(
        crate::services::compaction::response_output_text(&result).as_deref(),
        Some("short destination answer")
    );

    let requests = upstream.requests_through_final();
    assert!(
        (2..=5).contains(&requests.len()),
        "expected bounded map/reduce summaries plus one destination inference, got {}",
        requests.len()
    );
    for request in &requests {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.headers["authorization"], "Bearer upstream-secret");
        assert!(request.headers.contains_key("anthropic-version"));
        assert_eq!(request.body["model"], "sonnet");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(request.body["tools"][0]["name"], "StructuredOutput");
    }
    assert!(
        requests[..requests.len() - 1].iter().any(|request| request
            .transcript
            .to_string()
            .contains("Create a structured portable checkpoint")),
        "a summary transcript must reach the actual CLI Messages request"
    );
    let final_request = requests.last().expect("final destination inference");
    assert!(final_request.final_turn);
    assert!(
        final_request
            .transcript
            .to_string()
            .contains("portable checkpoint"),
        "the compacted checkpoint must reach the short destination inference"
    );
    assert_eq!(
        final_request.transcript["model"], "demo/short",
        "the configured short destination remains the client model"
    );

    server
        .shutdown()
        .expect("shutdown short-destination server");
}
