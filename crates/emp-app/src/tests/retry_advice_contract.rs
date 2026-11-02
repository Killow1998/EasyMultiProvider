//! Retry advice must survive the public HTTP and WebSocket boundaries.
use super::*;

fn client_failure(
    server: &ServerHandle,
    session: &str,
    body: &mut Value,
    delivery: &str,
) -> (u16, Value, Vec<Value>) {
    let mut events = Vec::new();
    let status = if delivery == "websocket" {
        let headers = BTreeMap::from([(
            "x-emp-session".to_owned(),
            session.trim_start_matches("X-EMP-Session: ").to_owned(),
        )]);
        let mut socket = emp_transport::ClientWebSocket::connect(
            &format!("ws://{}/v1/responses", server.local_addr()),
            &headers,
            Duration::from_secs(5),
        )
        .unwrap();
        body["type"] = json!("response.create");
        socket.send_json(body).unwrap();
        loop {
            let event = socket.receive_json().unwrap().expect("WebSocket event");
            let terminal = matches!(event["type"].as_str(), Some("error" | "response.failed"));
            events.push(event);
            assert!(events.len() <= 20, "missing terminal failure");
            if terminal {
                break;
            }
        }
        let last = events.last().unwrap();
        last["status"]
            .as_u64()
            .or_else(|| last["error"]["status"].as_u64())
            .or_else(|| last["response"]["error"]["status"].as_u64())
            .unwrap_or(502) as u16
    } else {
        let wire = post_stream(
            server,
            "/v1/responses",
            &serde_json::to_vec(body).unwrap(),
            &[session],
        );
        let status = wire.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = wire.split("\r\n\r\n").nth(1).unwrap();
        if status == 200 {
            events.extend(
                body.lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .filter(|line| *line != "[DONE]")
                    .map(|line| serde_json::from_str(line).unwrap()),
            );
        } else {
            events.push(serde_json::from_str(body).unwrap());
        }
        status
    };
    let last = events.last().expect("client failure");
    let detail = last
        .get("error")
        .or_else(|| last.pointer("/response/error"))
        .expect("error detail")
        .clone();
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("private upstream detail")
    );
    (status, detail, events)
}

#[test]
fn upstream_refusals_are_classified_without_replay_across_sources_and_transports() {
    for (status, code, detail, hint) in [
        (400, "model_not_found", "private upstream detail", "model"),
        (403, "model_not_found", "private upstream detail", "model"),
        (404, "model_not_found", "private upstream detail", "model"),
        (
            400,
            "upstream_capacity",
            "Concurrency limit exceeded for account; private upstream detail",
            "you can retry your request",
        ),
        (
            409,
            "upstream_capacity",
            "Too many pending requests; private upstream detail",
            "you can retry your request",
        ),
        (
            422,
            "upstream_capacity",
            "当前并发请求过多，请稍后重试；private upstream detail",
            "you can retry your request",
        ),
        (
            400,
            "request_too_large",
            "Request body is 40.1 MB, over the 32 MB limit; private upstream detail",
            "byte-size limit",
        ),
        (
            413,
            "request_too_large",
            "Payload too large; private upstream detail",
            "byte-size limit",
        ),
    ] {
        for protocol in [
            "native",
            "responses",
            "chat_completions",
            "anthropic_messages",
            "auto",
        ] {
            for delivery in ["http", "sse", "websocket"] {
                // Native WebSocket requires an upstream WebSocket, not this HTTP fixture.
                if protocol == "native" && delivery == "websocket" {
                    continue;
                }
                let upstream =
                    OneShotUpstream::start_wire(
                        status,
                        "application/json",
                        None,
                        vec![serde_json::to_vec(&json!({"error":{
                        "code":if code == "model_not_found" { code } else { "upstream_error" },
                        "message":detail,
                    }})).unwrap()],
                    );
                let (_directory, server) = if protocol == "native" {
                    let (directory, server, _) = auto_review_contract::make_review_server(
                        &upstream.base_url(),
                        &[],
                        Some("regular"),
                    );
                    (directory, server)
                } else {
                    configured_protocol_server(&upstream.base_url(), protocol, "api_key")
                };
                let session = session_header(&server);
                if protocol != "native" {
                    server
                        .state
                        .backend
                        .configuration
                        .test_config()
                        .lock()
                        .unwrap()["models"][0]["input_modalities"] = json!(["text", "image"]);
                }
                let model = if protocol == "native" {
                    "codex-auto-review"
                } else {
                    "demo/model"
                };
                let image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Y9Z0l0AAAAASUVORK5CYII=";
                let mut body = json!({"model":model,"stream":delivery != "http",
                    "input":[
                        {"role":"user","content":[{"type":"input_text","text":"fixture-history"},{"type":"input_image","image_url":format!("data:image/png;base64,{image}")}]},
                        {"type":"function_call","call_id":"call_fixture","name":"lookup","arguments":"{}"},
                        {"type":"function_call_output","call_id":"call_fixture","output":"fixture-exact-tool-output"},
                        {"role":"user","content":"continue"}
                    ],
                    "tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]
                });
                let (received_status, failure, _) =
                    client_failure(&server, &session, &mut body, delivery);
                assert_eq!(received_status, status, "{protocol}/{delivery}: {failure}");
                assert_eq!(failure["code"], code, "{protocol}/{delivery}: {failure}");
                assert!(
                    failure["message"].as_str().unwrap().contains(hint),
                    "{protocol}/{delivery}/{status}: {failure}"
                );
                assert!(!failure.to_string().contains("private upstream detail"));
                let forwarded = upstream.observed().2.to_string();
                for original in [image, "fixture-history", "fixture-exact-tool-output"] {
                    assert!(
                        forwarded.contains(original),
                        "{protocol}/{delivery}: original input lost"
                    );
                }
                let report = recorded_calls(&server, &session);
                let call = &report["records"][0];
                assert_eq!(call["attempts"], 1, "{protocol}/{delivery}: {call}");
                assert_eq!(call["retries"], json!([]));
                assert_eq!(
                    call["failure_reason"], code,
                    "{protocol}/{delivery}: {call}"
                );
                server.shutdown().unwrap();
            }
        }
    }
}

#[test]
fn streamed_refusals_keep_advice_and_never_replay_partial_output_or_tools() {
    for protocol in [
        "native",
        "responses",
        "chat_completions",
        "anthropic_messages",
    ] {
        for delivery in ["sse", "websocket"] {
            if protocol == "native" && delivery == "websocket" {
                continue;
            }
            for phase in ["before", "text", "tool"] {
                for (status, upstream_code, code, hint) in [
                    (
                        None,
                        "overloaded_error",
                        "upstream_capacity",
                        "you can retry your request",
                    ),
                    (
                        Some(413),
                        "request_too_large",
                        "request_too_large",
                        "byte-size limit",
                    ),
                ] {
                    let mut events = Vec::new();
                    if phase != "before" {
                        match protocol {
                            "chat_completions" => {
                                let delta = if phase == "text" {
                                    json!({"content":"partial"})
                                } else {
                                    json!({"tool_calls":[{"index":0,"id":"call_lookup","type":"function",
                                        "function":{"name":"lookup","arguments":"{}"}}]})
                                };
                                events.push(json!({"id":"chat_fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]}));
                            }
                            "anthropic_messages" => {
                                events.push(json!({"type":"message_start","message":{"id":"msg_fixture","usage":{"input_tokens":17}}}));
                                if phase == "text" {
                                    events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                                    events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}));
                                } else {
                                    events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_lookup","name":"lookup","input":{}}}));
                                    events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}));
                                }
                            }
                            _ => {
                                events.push(json!({"type":"response.created","response":{"id":"resp_fixture","status":"in_progress","output":[]}}));
                                if phase == "text" {
                                    events.push(json!({"type":"response.output_text.delta","item_id":"msg_fixture","output_index":0,"content_index":0,"delta":"partial"}));
                                } else {
                                    events.push(json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_fixture","type":"function_call","call_id":"call_lookup","name":"lookup","arguments":"{}"}}));
                                }
                            }
                        }
                    }
                    let error = json!({"type":upstream_code,"message":"private upstream detail","status":status});
                    events.push(if matches!(protocol, "native" | "responses") {
                        json!({"type":"response.failed","response":{"id":"resp_fixture","status":"failed","output":[],
                            "usage":{"input_tokens":17,"output_tokens":1,"total_tokens":18},"error":error}})
                    } else { json!({"type":"error","error":error}) });
                    let wire = events
                        .iter()
                        .map(|event| format!("data: {event}\n\n"))
                        .collect::<String>();
                    let upstream = OneShotUpstream::start_sse(vec![wire.into_bytes()]);
                    let (_directory, server) = if protocol == "native" {
                        let (directory, server, _) = auto_review_contract::make_review_server(
                            &upstream.base_url(),
                            &[],
                            Some("regular"),
                        );
                        (directory, server)
                    } else {
                        configured_protocol_server(&upstream.base_url(), protocol, "api_key")
                    };
                    let session = session_header(&server);
                    let mut body = json!({"model":if protocol == "native" { "codex-auto-review" } else { "demo/model" },
                        "input":"fixture","stream":true,"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]});
                    let (_, failure, received) =
                        client_failure(&server, &session, &mut body, delivery);
                    assert_eq!(
                        failure["code"], code,
                        "{protocol}/{phase}/{delivery}: {failure}"
                    );
                    assert!(
                        failure["message"].as_str().unwrap().contains(hint),
                        "{failure}"
                    );
                    if phase == "tool" && protocol == "chat_completions" {
                        // Chat buffers tool calls until [DONE]; this fragment has not dispatched a tool.
                        assert!(
                            !received
                                .iter()
                                .any(|event| event["item"]["type"] == "function_call")
                        );
                    } else if phase != "before" {
                        assert!(
                            received.iter().any(|event| if phase == "text" {
                                event["type"] == "response.output_text.delta"
                                    && event["delta"] == "partial"
                            } else {
                                event["item"]["type"] == "function_call"
                            }),
                            "{protocol}/{phase}: {received:?}"
                        );
                        if matches!(protocol, "native" | "responses") {
                            let terminal = received.last().unwrap();
                            assert_eq!(terminal["response"]["id"], "resp_fixture");
                            assert_eq!(terminal["response"]["usage"]["input_tokens"], 17);
                        }
                    }
                    upstream.observed();
                    let report = recorded_calls(&server, &session);
                    let call = &report["records"][0];
                    assert_eq!(
                        call["attempts"], 1,
                        "{protocol}/{phase}/{delivery}: {report}"
                    );
                    assert_eq!(call["retries"], json!([]));
                    assert_eq!(call["failure_reason"], code, "{protocol}/{phase}: {call}");
                    assert_eq!(call["error_class"], code, "{protocol}/{phase}: {call}");
                    if matches!(protocol, "native" | "responses") {
                        assert_eq!(call["input_tokens"], 17, "{protocol}/{phase}: {call}");
                        assert_eq!(call["output_tokens"], 1, "{protocol}/{phase}: {call}");
                    }
                    server.shutdown().unwrap();
                }
            }
        }
    }
}

#[test]
fn upstream_retry_advice_reaches_http_and_websocket_clients() {
    for status in [429, 503] {
        for websocket in [false, true] {
            // Above EMP's local retry ceiling: expose the advice to Codex
            // immediately without waiting or replaying the inference.
            let upstream = OneShotUpstream::start_wire(
                status,
                "application/json",
                Some(301),
                vec![br#"{"error":{"message":"private upstream detail"}}"#.to_vec()],
            );
            let (_directory, server) = configured_server(&upstream.base_url());
            let session = session_header(&server);
            let body = json!({"model":"demo/model","input":"fixture","stream":true});
            if websocket {
                let headers = BTreeMap::from([(
                    "x-emp-session".to_owned(),
                    session.trim_start_matches("X-EMP-Session: ").to_owned(),
                )]);
                let mut socket = emp_transport::ClientWebSocket::connect(
                    &format!("ws://{}/v1/responses", server.local_addr()),
                    &headers,
                    Duration::from_secs(5),
                )
                .unwrap();
                let mut body = body;
                body["type"] = json!("response.create");
                socket.send_json(&body).unwrap();
                let mut failure = None;
                for _ in 0..4 {
                    let event = socket.receive_json().unwrap().expect("WebSocket event");
                    if event["type"] == "error" {
                        failure = Some(event);
                        break;
                    }
                }
                let failure = failure.expect("upstream failure");
                assert_eq!(failure["status"], status);
                assert_eq!(failure["error"]["retry_after_seconds"], 301);
                assert_eq!(failure["error"]["headers"]["Retry-After"], "301");
                assert!(!failure.to_string().contains("private upstream detail"));
            } else {
                let wire = post(
                    &server,
                    "/v1/responses",
                    &serde_json::to_vec(&body).unwrap(),
                    &[&session],
                );
                assert_eq!(
                    wire.split_whitespace().nth(1),
                    Some(status.to_string().as_str())
                );
                assert!(wire.contains("Retry-After: 301\r\n"), "{wire}");
                assert!(!wire.contains("private upstream detail"));
            }
            assert_eq!(upstream.observed().2["model"], "upstream-model");
            let report = request(&server, "/api/calls", &[&session]);
            let report: Value =
                serde_json::from_str(report.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            let call = &report["records"][0];
            assert_eq!(call["retry_after_seconds"], 301);
            assert_eq!(call["attempts"], 1);
            assert_eq!(call["retries"], json!([]));
            assert_eq!(call["state"], "failed");
            assert!(!report.to_string().contains("private upstream detail"));
            server.shutdown().unwrap();
        }
    }
}
