//! Real server contract tests.
use super::*;

fn upstream_sse(events: &[Value]) -> Vec<u8> {
    let mut wire = Vec::new();
    for event in events {
        wire.extend_from_slice(b"data: ");
        wire.extend_from_slice(
            serde_json::to_string(event)
                .expect("upstream SSE JSON")
                .as_bytes(),
        );
        wire.extend_from_slice(b"\n\n");
    }
    wire.extend_from_slice(b"data: [DONE]\n\n");
    wire
}

fn assert_stream_protocol(protocol: &str, auth_mode: &str, expected_path: &str, events: &[Value]) {
    let upstream = OneShotUpstream::start_sse(vec![upstream_sse(events)]);
    let (_directory, server) =
        configured_protocol_server(&upstream.base_url(), protocol, auth_mode);
    let request_body = serde_json::to_vec(&json!({
        "model": "demo/model",
        "input": [{
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
        "stream": true
    }))
    .expect("request JSON");
    let response = post_stream(
        &server,
        "/v1/responses",
        &request_body,
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: text/event-stream\r\n"));
    assert!(response.contains("X-Frame-Options: DENY\r\n"));
    assert!(response.contains("Content-Security-Policy: frame-ancestors 'none'\r\n"));
    assert!(response.contains("X-Content-Type-Options: nosniff\r\n"));
    assert!(!response.contains("Content-Length:"));
    assert!(response.contains("event: response.created\n"), "{response}");
    assert!(
        response.contains("event: response.output_text.delta\n"),
        "{response}"
    );
    assert!(
        response.contains("event: response.completed\n"),
        "{response}"
    );
    let (path, headers, upstream_body) = upstream.observed();
    assert_eq!(path, expected_path);
    if auth_mode == "anthropic_api_key" {
        assert_eq!(headers["x-api-key"], "upstream-secret");
    } else {
        assert_eq!(headers["authorization"], "Bearer upstream-secret");
    }
    assert_eq!(upstream_body["model"], "upstream-model");
    assert_eq!(upstream_body["stream"], true);
    server.shutdown().expect("shutdown");
}

#[test]
fn streamed_external_protocols_cross_the_real_server_boundary() {
    let chat = [
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    ];
    assert_stream_protocol("chat_completions", "api_key", "/v1/chat/completions", &chat);

    let anthropic = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":3}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"answer"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        json!({"type":"message_stop"}),
    ];
    assert_stream_protocol(
        "anthropic_messages",
        "anthropic_api_key",
        "/v1/messages",
        &anthropic,
    );

    let response = json!({
        "id":"responses_upstream", "object":"response", "status":"completed",
        "model":"upstream-model",
        "output":[{"id":"msg_visible","type":"message","status":"completed",
            "role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}],
        "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
    });
    let responses = [
        json!({"type":"response.created","response":{"id":"responses_upstream","object":"response","status":"in_progress","model":"upstream-model","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_visible","type":"message","status":"in_progress","role":"assistant","content":[]}}),
        json!({"type":"response.content_part.added","item_id":"msg_visible","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_visible","output_index":0,"content_index":0,"delta":"answer"}),
        json!({"type":"response.output_text.done","item_id":"msg_visible","output_index":0,"content_index":0,"text":"answer"}),
        json!({"type":"response.content_part.done","item_id":"msg_visible","output_index":0,"content_index":0,"part":{"type":"output_text","text":"answer","annotations":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_visible","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}}),
        json!({"type":"response.completed","response":response}),
    ];
    assert_stream_protocol("responses", "api_key", "/v1/responses", &responses);
}

#[test]
fn streaming_flushes_before_upstream_eof() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    let first_event = json!({
        "id":"chat_upstream",
        "choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]
    });
    let first = format!(
        "data: {}\n\n",
        serde_json::to_string(&first_event).expect("first upstream event")
    )
    .into_bytes();
    let last = upstream_sse(&[json!({
        "id":"chat_upstream",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
    })]);
    let content_length = first.len() + last.len();
    let (first_sent, first_ready) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream");
        let _ = receive_upstream_request(&mut stream);
        stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .expect("write upstream response head");
        stream.write_all(&first).expect("write first SSE event");
        stream.flush().expect("flush first SSE event");
        first_sent.send(()).expect("announce first SSE event");
        released
            .recv_timeout(Duration::from_secs(3))
            .expect("release terminal SSE event");
        stream.write_all(&last).expect("write terminal SSE event");
    });
    let (_directory, server) = configured_server(&format!("http://{address}/v1"));
    let body = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("request JSON");
    let mut downstream =
        open_post_stream(&server, "/v1/responses", &body, &[&session_header(&server)]);
    first_ready
        .recv_timeout(Duration::from_secs(2))
        .expect("upstream first event");
    downstream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("downstream timeout");
    let mut response = Vec::new();
    while !response
        .windows(b"event: response.output_text.delta\n".len())
        .any(|window| window == b"event: response.output_text.delta\n")
    {
        let mut chunk = [0_u8; 4096];
        let count = downstream.read(&mut chunk).expect("incremental SSE read");
        assert!(count > 0, "downstream ended before visible output");
        response.extend_from_slice(&chunk[..count]);
    }
    release.send(()).expect("release terminal SSE event");
    downstream
        .read_to_end(&mut response)
        .expect("finish downstream SSE");
    let response = String::from_utf8(response).expect("UTF-8 SSE response");
    assert!(response.contains("event: response.completed\n"));
    server.shutdown().expect("shutdown");
    worker.join().expect("join upstream");
}

#[test]
fn downstream_disconnect_cancels_a_waiting_upstream_stream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    let (head_sent, head_ready) = mpsc::sync_channel(1);
    let (closed_sender, closed) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream");
        let _ = receive_upstream_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .expect("write upstream response head");
        stream.flush().expect("flush upstream response head");
        head_sent.send(()).expect("announce upstream head");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("upstream close timeout");
        let mut byte = [0_u8; 1];
        let closed_by_emp = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        closed_sender
            .send(closed_by_emp)
            .expect("report upstream cancellation");
    });
    let (_directory, server) = configured_server(&format!("http://{address}/v1"));
    let body = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("request JSON");
    let downstream = open_post_stream(&server, "/v1/responses", &body, &[&session_header(&server)]);
    head_ready
        .recv_timeout(Duration::from_secs(2))
        .expect("upstream response head");
    drop(downstream);
    assert!(
        closed
            .recv_timeout(Duration::from_secs(3))
            .expect("upstream cancellation result"),
        "EMP kept the upstream stream open after its downstream disconnected"
    );
    server.shutdown().expect("shutdown");
    worker.join().expect("join upstream");
}

#[test]
fn stream_errors_keep_pre_and_post_output_boundaries() {
    // The retry budget is three attempts, so the fixture answers every
    // connection with the same 429 before the budget is exhausted.
    let upstream = OneShotUpstream::start_repeated_wire(
        429,
        "application/json",
        Some(6),
        3,
        vec![
            serde_json::to_vec(&json!({"error":{"message":"provider detail must not escape"}}))
                .expect("upstream error JSON"),
        ],
    );
    let (_directory, server) = configured_server(&upstream.base_url());
    let body = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("request JSON");
    let response = post(&server, "/v1/responses", &body, &[&session_header(&server)]);
    assert!(response.starts_with("HTTP/1.1 429 Too Many Requests\r\n"));
    assert!(response.contains("Retry-After: 6\r\n"));
    assert!(response.contains("\"type\":\"rate_limit\""));
    assert!(response.contains("\"code\":\"rate_limit_exceeded\""));
    assert!(!response.contains("provider detail must not escape"));
    server.shutdown().expect("shutdown");

    let partial = format!(
        "data: {}\n\n",
        serde_json::to_string(&json!({
            "id":"chat_upstream",
            "choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]
        }))
        .expect("partial upstream event")
    )
    .into_bytes();
    let upstream = OneShotUpstream::start_sse(vec![partial]);
    let (_directory, server) = configured_server(&upstream.base_url());
    let response = post_stream(&server, "/v1/responses", &body, &[&session_header(&server)]);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains("event: response.output_text.delta\n"));
    assert!(response.contains("event: response.failed\n"));
    assert!(response.contains("\"status\": 502"));
    assert!(!response.contains("event: response.completed\n"));
    server.shutdown().expect("shutdown");
}

#[test]
fn auto_protocol_falls_back_only_after_explicit_endpoint_rejection() {
    let complete_body = serde_json::to_vec(&json!({
            "id":"responses_upstream", "object":"response", "status":"completed",
            "model":"upstream-model",
            "output":[{"id":"msg_visible","type":"message","status":"completed",
                "role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}],
            "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
        }))
        .expect("complete upstream body");
    let (base_url, paths, worker) = fallback_upstream("application/json", complete_body);
    let (directory, server) = configured_protocol_server(&base_url, "auto", "api_key");
    let complete_request = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":false,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("complete request JSON");
    let response = post(
        &server,
        "/v1/responses",
        &complete_request,
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("\"text\":\"answer\""));
    assert_eq!(
        [
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
        ],
        ["/v1/chat/completions", "/v1/responses"]
    );
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    let receipt = &snapshot["requests"][0];
    assert_eq!(receipt["attempts"], 2);
    assert_eq!(receipt["state"], "completed");
    assert_eq!(receipt["response_model"], "upstream-model");
    assert_eq!(receipt["retries"][0]["reason"], "protocol_rejection");
    assert_saved_protocol_observation(&directory, &server, "responses");
    server.shutdown().expect("shutdown");
    worker.join().expect("join complete fallback upstream");

    let terminal = json!({
        "id":"responses_upstream", "object":"response", "status":"completed",
        "model":"upstream-model",
        "output":[{"id":"msg_visible","type":"message","status":"completed",
            "role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}]}],
        "usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}
    });
    let stream_body = upstream_sse(&[
        json!({"type":"response.created","response":{"id":"responses_upstream","object":"response","status":"in_progress","model":"upstream-model","output":[]}}),
        json!({"type":"response.output_text.delta","item_id":"msg_visible","output_index":0,"content_index":0,"delta":"answer"}),
        json!({"type":"response.completed","response":terminal}),
    ]);
    let (base_url, paths, worker) = fallback_upstream("text/event-stream", stream_body);
    let (directory, server) = configured_protocol_server(&base_url, "auto", "api_key");
    let stream_request = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("stream request JSON");
    let response = post_stream(
        &server,
        "/v1/responses",
        &stream_request,
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("event: response.output_text.delta\n"));
    assert!(response.contains("event: response.completed\n"));
    assert_eq!(
        [
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
        ],
        ["/v1/chat/completions", "/v1/responses"]
    );
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    let receipt = &snapshot["requests"][0];
    assert_eq!(receipt["attempts"], 2);
    assert_eq!(receipt["state"], "completed");
    assert_eq!(receipt["response_model"], "upstream-model");
    assert_eq!(receipt["retries"][0]["reason"], "protocol_rejection");
    assert_saved_protocol_observation(&directory, &server, "responses");
    server.shutdown().expect("shutdown");
    worker.join().expect("join stream fallback upstream");
}

#[test]
fn external_pre_output_retry_is_single_and_route_local() {
    let complete_body = serde_json::to_vec(&json!({
            "id":"chat_upstream", "model":"upstream-model", "object":"chat.completion",
            "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}
        }))
        .expect("complete Chat body");
    let (base_url, paths, worker) =
        two_attempt_upstream(429, Some(0), "application/json", complete_body);
    let (directory, server) = configured_server(&base_url);
    let complete_request = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":false,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("complete request JSON");
    let response = post(
        &server,
        "/v1/responses",
        &complete_request,
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("\"text\":\"answer\""));
    assert_eq!(
        [
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
        ],
        ["/v1/chat/completions", "/v1/chat/completions"]
    );
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    let receipt = &snapshot["requests"][0];
    assert_eq!(receipt["attempts"], 2);
    assert_eq!(receipt["state"], "completed");
    assert_eq!(receipt["retries"][0]["reason"], "rate_limit");
    server.shutdown().expect("shutdown");
    worker.join().expect("join complete retry upstream");
    assert_retry_journal(&directory);

    let stream_body = upstream_sse(&[
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
        json!({"id":"chat_upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}),
    ]);
    let (base_url, paths, worker) =
        two_attempt_upstream(429, Some(0), "text/event-stream", stream_body);
    let (directory, server) = configured_server(&base_url);
    let stream_request = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("stream request JSON");
    let response = post_stream(
        &server,
        "/v1/responses",
        &stream_request,
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("event: response.completed\n"));
    assert_eq!(
        [
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
            paths.recv_timeout(Duration::from_secs(2)).unwrap(),
        ],
        ["/v1/chat/completions", "/v1/chat/completions"]
    );
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    let receipt = &snapshot["requests"][0];
    assert_eq!(receipt["attempts"], 2);
    assert_eq!(receipt["state"], "completed");
    assert_eq!(receipt["retries"][0]["reason"], "rate_limit");
    server.shutdown().expect("shutdown");
    worker.join().expect("join stream retry upstream");
    assert_retry_journal(&directory);
}

fn assert_retry_journal(directory: &TempDir) {
    let records = super::internal_events_contract::journal(directory.path());
    let retries: Vec<_> = records
        .iter()
        .filter(|record| record["event"] == "model_retry_scheduled")
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0]["fields"]["status"], 429);
    assert_eq!(retries[0]["fields"]["protocol_fallback"], false);
    let observations: Vec<_> = records
        .iter()
        .filter(|record| record["event"] == "route_observation")
        .collect();
    assert_eq!(
        observations.len(),
        1,
        "retry must not double-count the completed turn"
    );
    assert_eq!(
        retries[0]["fields"]["request_id"],
        observations[0]["fields"]["request_id"]
    );
}

#[test]
fn stream_boundary_frames_and_activity_are_directly_asserted() {
    let events = [
        json!({"type":"response.created","response":{"status":"in_progress","output":[]}}),
        json!({"type":"response.output_text.delta","delta":"回答, key: value"}),
        json!({"type":"response.output_item.added","item":{"id":"call_1","type":"function_call"}}),
    ];
    // Frames round-trip: the event name leads and the payload is valid JSON
    // with a trailing blank line, so any SSE client can parse them.
    for event in &events {
        let frame =
            String::from_utf8(sse_frame(event["type"].as_str().unwrap(), event).unwrap()).unwrap();
        let (event_name, data) = frame
            .strip_prefix("event: ")
            .and_then(|frame| frame.split_once("\ndata: "))
            .expect("SSE event and data lines");
        assert_eq!(event_name, event["type"]);
        assert!(frame.ends_with("\n\n"));
        let parsed: Value = serde_json::from_str(data.trim_end_matches("\n\n")).expect("SSE JSON");
        assert_eq!(parsed, *event);
    }
    // Activity marks text deltas as output and tool items as tool calls.
    let observed: Vec<bool> = events
        .iter()
        .map(|event| {
            let (output, tool) = stream_event_activity(event);
            output || tool
        })
        .collect();
    assert_eq!(observed, [false, true, true]);
}

#[test]
fn response_body_errors_keep_the_stable_status_boundary() {
    let (_directory, server) = test_server();
    let cookie = session_header(&server);
    let wrong_type = post(
        &server,
        "/v1/responses",
        b"{}",
        &[&cookie, "Content-Type: text/plain"],
    );
    assert!(wrong_type.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(wrong_type.contains("Content-Type must be application/json"));

    let non_object = post(&server, "/v1/responses", b"[]", &[&cookie]);
    assert!(non_object.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(non_object.contains("request body must be a JSON object"));

    let missing_model = post(&server, "/v1/responses", b"{}", &[&cookie]);
    assert!(missing_model.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(missing_model.contains("request.model is required"));

    let unknown_stream = post(
        &server,
        "/v1/responses",
        br#"{"model":"anything","stream":true}"#,
        &[&cookie],
    );
    assert!(unknown_stream.starts_with("HTTP/1.1 404 Not Found\r\n"));
    server.shutdown().expect("shutdown");

    let (_directory, server) = configured_server("http://127.0.0.1:9/v1");
    let cookie = session_header(&server);
    let stream = post(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","stream":true}"#,
        &[&cookie],
    );
    assert!(stream.starts_with("HTTP/1.1 503 Service Unavailable\r\n"));
    assert!(stream.contains("network"));
    server.shutdown().expect("shutdown");
}
