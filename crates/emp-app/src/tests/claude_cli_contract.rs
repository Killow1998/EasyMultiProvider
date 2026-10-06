//! Installed-Claude end-to-end checks use a fake Anthropic CPA endpoint.
use super::*;

fn claude_server(base_url: &str) -> (TempDir, ServerHandle) {
    claude_server_with_model(base_url, "sonnet")
}

fn claude_server_with_model(base_url: &str, upstream_model: &str) -> (TempDir, ServerHandle) {
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
                "id":"demo/model", "provider":"demo", "upstream_id":upstream_model,
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
    let transcript: Value = serde_json::from_str(text).expect("serialized transcript JSON");
    transcript
        .get("codex_responses_metadata")
        .cloned()
        .unwrap_or(transcript)
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
                let content = match &body["messages"][0]["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let final_turn = content.contains("active destination request");
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
fn installed_cli_claude_46_rejects_none_then_preserves_history_with_caller_selected_medium() {
    assert!(emp_codex::installed_cli::resolve_claude_cli().is_some());
    for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
        let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
            "answer":"same history accepted", "tool_calls":[]
        }))]);
        let (_directory, server) = claude_server_with_model(&upstream.base_url(), model);
        let mut request = json!({
            "model":"demo/model", "stream":false, "reasoning":{"effort":"none"},
            "input":[
                {"role":"user","content":"synthetic first turn"},
                {"type":"function_call","call_id":"previous","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"previous","output":"exact result\nquote \" café"},
                {"role":"user","content":"continue with the same history"}
            ],
            "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]
        });
        let original_input = request["input"].clone();
        let rejected = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&request).unwrap(),
            &[&session_header(&server)],
        );
        assert!(rejected.starts_with("HTTP/1.1 400 "), "{rejected}");
        let error: Value =
            serde_json::from_str(rejected.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(error["error"]["code"], "unsupported_reasoning_effort");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("EMP received reasoning.effort=\"none\"")
        );
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Select a supported effort level offered for this model")
        );
        assert!(
            upstream.observed.try_recv().is_err(),
            "rejected effort must not reach CPA"
        );

        // The caller explicitly changes its effort; EMP never rewrites none or retries it.
        request["reasoning"]["effort"] = json!("medium");
        let accepted = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&request).unwrap(),
            &[&session_header(&server)],
        );
        assert!(accepted.starts_with("HTTP/1.1 200 "), "{accepted}");
        let result: Value =
            serde_json::from_str(accepted.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            result["output"][0]["content"][0]["text"],
            "same history accepted"
        );
        let (_, _, forwarded) = upstream.observed();
        assert_eq!(forwarded["model"], model);
        assert_eq!(forwarded["output_config"]["effort"], "medium");
        let transcript = user_transcript(&forwarded);
        assert_eq!(transcript["input"], original_input);
        assert_eq!(transcript["tools"], request["tools"]);
        assert_eq!(transcript["reasoning"]["effort"], "medium");
        server.shutdown().expect("shutdown 4.6 server");
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
    assert_eq!(upstream_body["output_config"]["effort"], "low");
    assert!(headers.contains_key("anthropic-version"), "{headers:?}");
    assert_eq!(upstream_body["stream"], true);
    assert_eq!(upstream_body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(upstream_body["messages"][0]["role"], "user");
    // The CLI may send its date reminder as text or as an empty system
    // message carrying only the already-matched effort. Neither shape may
    // alter the user transcript or remove EMP's system instruction.
    assert!(upstream_body["system"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block["text"]
                .as_str()
                .is_some_and(|text| text.contains("one-request compatibility bridge"))
        })
    }));
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
        &serde_json::to_vec(&json!({"model":"demo/model","stream":true,"input":"stream request","reasoning":{"effort":null}}))
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
    assert_eq!(
        user_transcript(&body)["reasoning"].get("effort"),
        Some(&Value::Null)
    );
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
fn installed_cli_forwards_images_and_documents_with_ordered_tool_history() {
    assert_cli_forwards_images_and_documents_with_ordered_tool_history("sonnet");
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_opus_55_forwards_images_and_documents_with_ordered_tool_history() {
    assert_cli_forwards_images_and_documents_with_ordered_tool_history("claude-opus-5-5");
}

fn assert_cli_forwards_images_and_documents_with_ordered_tool_history(model: &str) {
    assert!(
        emp_codex::installed_cli::resolve_claude_cli().is_some(),
        "trusted Claude Code CLI must be on PATH"
    );

    let screenshot = synthetic_screenshot_png();
    let screenshot_base64 = STANDARD.encode(&screenshot);
    let inline = format!("data:image/png;base64,{screenshot_base64}");
    let pdf_bytes = minimal_pdf();
    let pdf_data = base64::engine::general_purpose::STANDARD.encode(&pdf_bytes);
    let text_file_bytes = b"tool text document";
    let pdf = format!("data:application/pdf;base64,{pdf_data}");
    let text_file = format!(
        "data:text/plain;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(text_file_bytes)
    );
    let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
        "answer":"multimodal path accepted", "tool_calls":[]
    }))]);
    let (_directory, server) = claude_server_with_model(&upstream.base_url(), model);
    let request_body = json!({
        "model":"demo/model",
        "stream":false,
        "reasoning":{"effort":"low"},
        "input":[
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"Inspect this inline image."},
                {"type":"input_image","image_url":inline,"detail":"high"},
                {"type":"input_file","filename":"tiny.pdf","file_data":pdf}
            ]},
            {"type":"function_call","call_id":"call-image-tool","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-image-tool","output":[
                {"type":"input_text","text":"ordinary payload JSON: {\"type\":\"input_image\",\"reasoning\":\"keep this field\"}"},
                {"type":"input_image","image_url":"https://images.invalid/tool-output.png"},
                {"type":"input_file","filename":"note.txt","file_data":text_file}
            ]},
            {"type":"message","role":"user","content":[
                {"type":"input_text","text":"Use the image and tool history for the follow-up."}
            ]}
        ],
        "tools":[{"type":"function","name":"inspect","parameters":{"type":"object","properties":{}}}]
    });
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&request_body).expect("multimodal Responses JSON"),
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).expect("Responses JSON");
    assert_eq!(
        crate::services::compaction::response_output_text(&result).as_deref(),
        Some("multimodal path accepted")
    );

    let (path, headers, messages_request) = upstream.observed();
    assert_eq!(path, "/v1/messages");
    assert_eq!(headers["authorization"], "Bearer upstream-secret");
    assert_eq!(messages_request["model"], model);
    assert_eq!(messages_request["stream"], true);
    assert_eq!(messages_request["messages"].as_array().unwrap().len(), 1);
    assert_eq!(messages_request["messages"][0]["role"], "user");
    // Bare CLI versions can omit the date reminder and send only empty
    // system-role effort metadata. The EMP bridge instruction must remain.
    assert!(
        messages_request["system"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| {
                block["text"].as_str().is_some_and(|text| {
                    text.starts_with("You are a one-request compatibility bridge for Codex.")
                })
            })
    );
    assert_eq!(messages_request["tools"].as_array().unwrap().len(), 1);
    assert_eq!(messages_request["tools"][0]["name"], "StructuredOutput");

    let content = messages_request["messages"][0]["content"]
        .as_array()
        .expect("ordered text and native media blocks");
    let images = content
        .iter()
        .enumerate()
        .filter(|(_, block)| block["type"] == "image")
        .collect::<Vec<_>>();
    assert_eq!(images.len(), 2, "both input images must reach CPA natively");
    assert_eq!(images[0].1["source"]["type"], "base64");
    assert_eq!(images[0].1["source"]["media_type"], "image/png");
    assert!(images[0].1["source"]["data"].as_str().unwrap().len() > 1000);
    let displayed_png = STANDARD
        .decode(images[0].1["source"]["data"].as_str().unwrap())
        .expect("CLI resized PNG base64");
    assert_eq!(
        (
            u32::from_be_bytes(displayed_png[16..20].try_into().unwrap()),
            u32::from_be_bytes(displayed_png[20..24].try_into().unwrap())
        ),
        (2000, 1125),
        "the installed CLI applies the verified screenshot resize"
    );
    assert!(content.iter().any(|block| {
        block["type"] == "text"
            && block["text"]
                == "[Image: original 2560x1440, displayed at 2000x1125. Multiply coordinates by 1.28 to map to original image.]"
    }));
    assert_eq!(images[1].1["source"]["type"], "url");
    assert_eq!(
        images[1].1["source"]["url"],
        "https://images.invalid/tool-output.png"
    );
    let documents = content
        .iter()
        .enumerate()
        .filter(|(_, block)| block["type"] == "document")
        .collect::<Vec<_>>();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].1["source"]["type"], "base64");
    assert_eq!(documents[0].1["source"]["media_type"], "application/pdf");
    assert_eq!(
        documents[0].1["source"]["data"],
        base64::engine::general_purpose::STANDARD.encode(&pdf_bytes),
        "the CLI must preserve PDF bytes"
    );

    for (block_index, block) in content.iter().enumerate() {
        let Some(record_text) = block["text"].as_str() else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(record_text) else {
            continue;
        };
        if record["content_part"]["type"] == "input_image"
            || record["tool_output_part"]["type"] == "input_image"
        {
            assert_eq!(content[block_index + 1]["type"], "image");
        }
        if record["content_part"]["type"] == "input_file"
            && record["content_part"]["filename"] == "tiny.pdf"
        {
            assert_eq!(content[block_index + 1]["type"], "document");
        }
        if record["tool_output_part"]["type"] == "input_file"
            && record["tool_output_part"]["filename"] == "note.txt"
        {
            assert_eq!(record["item"]["call_id"], "call-image-tool");
            assert_eq!(record["tool_output_part_index"], 2);
            assert_eq!(content[block_index + 1]["type"], "text");
            assert_eq!(
                content[block_index + 1]["text"],
                "Document note.txt:\ntool text document"
            );
        }
        if record["codex_item_index"] == 0 && record["content_part"]["type"] == "input_image" {
            assert_eq!(record["item"]["role"], "user");
            assert_eq!(record["content_part_index"], 1);
        }
        if record["item"]["call_id"] == "call-image-tool"
            && record["tool_output_part"]["type"] == "input_image"
        {
            assert_eq!(record["tool_output_part_index"], 1);
        }
        assert!(
            !record_text.contains(screenshot_base64.as_str()),
            "image bytes stay in the native block"
        );
    }
    assert!(content.iter().any(|block| {
        block["text"].as_str().is_some_and(|text| {
            text.contains("ordinary payload JSON") && text.contains("keep this field")
        })
    }));

    server.shutdown().expect("shutdown multimodal server");
}

fn minimal_pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] /Contents 4 0 R >>",
        "<< /Length 0 >>\nstream\nendstream",
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0_usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets.into_iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    pdf
}

fn synthetic_screenshot_png() -> Vec<u8> {
    const WIDTH: u32 = 2560;
    const HEIGHT: u32 = 1440;
    let mut scanlines = Vec::with_capacity((WIDTH as usize + 1) * HEIGHT as usize);
    for y in 0..HEIGHT {
        scanlines.push(0);
        for x in 0..WIDTH {
            scanlines.push((((x / 24) ^ (y / 20)) & 0xff) as u8);
        }
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(&scanlines)
        .expect("compress screenshot scanlines");
    let image_data = encoder.finish().expect("finish screenshot compression");

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&WIDTH.to_be_bytes());
    header.extend_from_slice(&HEIGHT.to_be_bytes());
    header.extend_from_slice(&[8, 0, 0, 0, 0]);
    append_png_chunk(&mut png, *b"IHDR", &header);
    append_png_chunk(&mut png, *b"IDAT", &image_data);
    append_png_chunk(&mut png, *b"IEND", &[]);
    png
}

fn append_png_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let mut checksum_data = Vec::with_capacity(kind.len() + data.len());
    checksum_data.extend_from_slice(&kind);
    checksum_data.extend_from_slice(data);
    png.extend_from_slice(&png_crc32(&checksum_data).to_be_bytes());
}

fn png_crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
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
    let pdf_bytes = minimal_pdf();
    let pdf = format!(
        "data:application/pdf;base64,{}",
        STANDARD.encode(&pdf_bytes)
    );
    for turn in 0..4 {
        let mut content = vec![json!({
            "type":"input_text",
            "text":format!("long-context user turn {turn}: {}", "u".repeat(2400))
        })];
        if turn == 0 {
            content.push(json!({
                "type":"input_file","filename":"checkpoint.pdf","file_data":pdf
            }));
        }
        input.push(json!({"type":"message","role":"user","content":content}));
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
    assert!(
        requests[..requests.len() - 1].iter().any(|request| {
            request.body["messages"][0]["content"]
                .as_array()
                .is_some_and(|content| {
                    content.iter().any(|block| {
                        block["type"] == "document"
                            && block["source"]["media_type"] == "application/pdf"
                            && block["source"]["data"] == STANDARD.encode(&pdf_bytes)
                    })
                })
        }),
        "automatic short-destination compaction must preserve native PDF bytes"
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
