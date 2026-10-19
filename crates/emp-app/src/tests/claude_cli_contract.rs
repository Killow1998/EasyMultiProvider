//! Installed-Claude end-to-end checks use a fake Anthropic CPA endpoint.
use super::*;

fn claude_server(base_url: &str) -> (TempDir, ServerHandle) {
    claude_server_with_model(base_url, "sonnet")
}

fn claude_server_with_model(base_url: &str, upstream_model: &str) -> (TempDir, ServerHandle) {
    claude_server_with_context(base_url, upstream_model, 0)
}

fn claude_server_with_context(
    base_url: &str,
    upstream_model: &str,
    context_window: u64,
) -> (TempDir, ServerHandle) {
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
                "context_window":context_window, "enabled":true
            }]
        }))
        .expect("encode config"),
    )
    .expect("write config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start Claude CLI server");
    (directory, server)
}

mod compaction;
mod media;
mod transcript_roundtrip;

pub(super) fn structured_messages_sse(proposal: &Value) -> Vec<u8> {
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
    let content = &messages["messages"]
        .as_array()
        .expect("Messages body")
        .iter()
        .find(|message| message["role"] == "user")
        .expect("user message")["content"];
    if let Some(text) = content.as_str() {
        return serde_json::from_str(text).expect("serialized transcript JSON");
    }
    let markers = content
        .as_array()
        .expect("content blocks")
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| serde_json::from_str::<Value>(block["text"].as_str().unwrap()).ok())
        .collect::<Vec<_>>();
    let Some(metadata) = markers[0].get("codex_responses_metadata") else {
        return markers[0].clone();
    };
    let mut transcript = metadata.clone();
    // Media contracts inspect native blocks separately; their text markers
    // contain redacted parts rather than complete original history items.
    if markers.iter().any(|marker| {
        marker.get("content_part_index").is_some() || marker.get("tool_output_part_index").is_some()
    }) {
        return transcript;
    }
    transcript["input"] = json!(
        markers[1..]
            .iter()
            .enumerate()
            .map(|(index, marker)| {
                assert_eq!(marker["codex_item_index"], index);
                assert!(marker.get("content_part_index").is_none());
                assert!(marker.get("tool_output_part_index").is_none());
                marker["item"].clone()
            })
            .collect::<Vec<_>>()
    );
    transcript
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_claude_46_rejects_none_then_preserves_history_with_caller_selected_medium() {
    assert!(emp_codex::installed_cli::resolve_claude_cli().is_some());
    for model in [
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-opus-4-6[1m]",
    ] {
        let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
            "answer":"same history accepted", "tool_calls":[]
        }))]);
        let (_directory, server) = claude_server_with_context(&upstream.base_url(), model, 300_000);
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
        let (_, headers, forwarded) = upstream.observed();
        assert_eq!(
            forwarded["model"],
            model.strip_suffix("[1m]").unwrap_or(model)
        );
        if model.ends_with("[1m]") {
            assert!(
                headers["anthropic-beta"].contains("context-1m"),
                "CLI context selection must survive the relay"
            );
        }
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
        "instructions":"unique top-level Codex instruction",
        "reasoning":{"effort":"low"},
        "input":[
            {"type":"message","role":"system","content":"unique Codex system policy"},
            {"type":"message","role":"developer","content":[{"type":"input_text","text":"unique Codex developer policy"}]},
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
            block["text"].as_str().is_some_and(|text| {
                text.contains("You answer the next turn for the Codex runtime.")
            })
        })
    }));
    assert_eq!(upstream_body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(upstream_body["tools"][0]["name"], "StructuredOutput");
    let transcript = user_transcript(&upstream_body);
    let system = upstream_body["system"].to_string();
    for instruction in [
        "unique top-level Codex instruction",
        "unique Codex system policy",
        "unique Codex developer policy",
    ] {
        assert!(
            system.contains(instruction),
            "instruction must reach the real system field"
        );
        assert!(
            !transcript.to_string().contains(instruction),
            "instruction must not be duplicated in user history"
        );
    }
    assert!(transcript.get("instructions").is_none());
    assert_eq!(transcript["model"], "demo/model");
    assert_eq!(transcript["input"][2]["call_id"], "call_previous");
    assert_eq!(
        transcript["input"][2]["output"],
        "exact prior tool result: newline\nquote \" café 🧪"
    );
    assert_eq!(transcript["tools"], history["tools"]);

    let report = request(
        &server,
        "/api/calls?start=0&end=9999999999",
        &[&session_header(&server)],
    );
    let report: Value = serde_json::from_str(report.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(report["total"], 1);
    assert_eq!(report["records"][0]["attempts"], 1);
    assert_eq!(
        report["records"][0]["resolved_protocol"],
        "anthropic_messages"
    );
    assert_eq!(report["records"][0]["input_tokens"], 10012);
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
fn installed_cli_preserves_large_tool_catalog_and_scoped_tool_round_trip() {
    assert!(emp_codex::installed_cli::resolve_claude_cli().is_some());
    let upstream = OneShotUpstream::start_repeated_wire(
        200,
        "text/event-stream",
        None,
        2,
        vec![structured_messages_sse(&json!({
            "answer":"", "tool_calls":[{
                "type":"function_call", "name":"functions.mcp__fixture_connector__operation_199", "namespace":"",
                "arguments":{"value":"fixture-only"}, "input":""
            }]
        }))],
    );
    let (_directory, server) = claude_server(&upstream.base_url());
    // This catalog made the request-specific schema alone exceed Windows'
    // complete command-line budget. All definitions must still reach the model.
    let tools = (0..200)
        .map(|index| {
            json!({
                "type":"function","name":format!("mcp__fixture_connector__operation_{index:03}"),
                "parameters":{"type":"object","properties":{"value":{"type":"string"}}}
            })
        })
        .collect::<Vec<_>>();
    let mut body = json!({"model":"demo/model","stream":false,
        "input":[{"role":"user","content":"propose the final offered tool"}],
        "tools":[{"type":"namespace","name":"functions","tools":tools}]
    });
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).unwrap(),
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(result["output"][0]["namespace"], "functions");
    assert_eq!(
        result["output"][0]["name"],
        "mcp__fixture_connector__operation_199"
    );
    assert_eq!(
        result["output"][0]["arguments"],
        "{\"value\":\"fixture-only\"}"
    );
    let (_, _, forwarded) = upstream.observed();
    assert_eq!(user_transcript(&forwarded)["tools"], body["tools"]);
    let first_content = forwarded["messages"][0]["content"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(forwarded["tools"].as_array().unwrap().len(), 1);
    assert_eq!(forwarded["tools"][0]["name"], "StructuredOutput");
    let schema = forwarded["tools"][0]["input_schema"].to_string();
    assert!(
        schema.encode_utf16().count() < 4096,
        "leave command-line headroom for CLI options and paths"
    );

    let call = result["output"][0].clone();
    body["input"].as_array_mut().unwrap().extend([
        call.clone(),
        json!({"type":"function_call_output","call_id":call["call_id"],"output":"exact synthetic tool result"}),
        json!({"role":"user","content":"continue"}),
    ]);
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).unwrap(),
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    // Synthetic replies test transport and projection, not model task quality.
    let (_, _, forwarded) = upstream.observed();
    let transcript = user_transcript(&forwarded);
    assert_eq!(transcript["input"], body["input"]);
    assert_eq!(transcript["tools"], body["tools"]);
    let later_content = forwarded["messages"][0]["content"].as_array().unwrap();
    assert!(
        later_content.len() > first_content.len(),
        "new history needs new cache boundaries"
    );
    for (mut first, later) in first_content.into_iter().zip(later_content) {
        let mut later = later.clone();
        first.as_object_mut().unwrap().remove("cache_control");
        later.as_object_mut().unwrap().remove("cache_control");
        assert_eq!(
            first, later,
            "installed CLI must preserve the cached history prefix"
        );
    }
    server.shutdown().unwrap();
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_preserves_published_claude_model_efforts() {
    assert!(emp_codex::installed_cli::resolve_claude_cli().is_some());
    for id in [
        "claude-haiku-5-5",
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-sonnet-4-6",
        "claude-opus-4-6",
        "claude-fable-5-1",
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-fable-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
    ] {
        let levels: &[&str] = if id.ends_with("4-6") {
            &["low", "medium", "high", "max"]
        } else {
            &["low", "medium", "high", "xhigh", "max"]
        };
        for level in levels {
            let upstream = OneShotUpstream::start_sse(vec![structured_messages_sse(&json!({
                "answer":"fixture answer","tool_calls":[]
            }))]);
            let (_directory, server) = claude_server_with_model(&upstream.base_url(), id);
            let body = json!({"model":"demo/model","stream":false,"input":"fixture only",
                "reasoning":{"effort":level}});
            let response = post(
                &server,
                "/v1/responses",
                &serde_json::to_vec(&body).unwrap(),
                &[&session_header(&server)],
            );
            assert!(
                response.starts_with("HTTP/1.1 200 OK\r\n"),
                "{id} {level}: {response}"
            );
            let (_, _, forwarded) = upstream.observed();
            assert_eq!(forwarded["model"], id, "preserve selected model");
            assert_eq!(
                forwarded["output_config"]["effort"], *level,
                "{id}: preserve effort"
            );
            server.shutdown().unwrap();
        }
        eprintln!("Verified {id}: {} effort levels", levels.len());
    }
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
    let mut created = 0;
    let mut error = None;
    for _ in 0..4 {
        let event = socket
            .receive_json()
            .expect("websocket error event")
            .expect("websocket remains open after a turn error");
        match event["type"].as_str() {
            Some("codex.response.metadata") => {}
            Some("response.created") => {
                created += 1;
                assert_eq!(event["response"]["status"], "in_progress");
                assert!(
                    event["response"]["id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty())
                );
            }
            Some("error") => {
                error = Some(event);
                break;
            }
            other => panic!("unexpected event before the rate-limit error: {other:?}"),
        }
    }
    assert_eq!(created, 1, "publish the response ID needed for steering");
    let event = error.expect("websocket rate-limit error after response creation");
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
