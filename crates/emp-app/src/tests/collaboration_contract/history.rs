//! Existing On-demand recovery covers encrypted parent history without session writes.
use super::*;
use std::fs;

// Bound the fake destination even if a policy assertion fails before dispatch.
fn history_upstream(responses: Vec<Value>) -> OneShotUpstream {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, observed) = mpsc::sync_channel(8);
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        for response in responses {
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                if Instant::now() >= deadline {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            };
            let request = receive_upstream_request(&mut stream);
            let _ = sender.send(request);
            let body = serde_json::to_vec(&response).unwrap();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    OneShotUpstream {
        address,
        observed,
        worker: Some(worker),
    }
}

#[test]
fn parent_encrypted_history_uses_existing_on_demand_mode_on_http_and_websocket() {
    let cipher = format!("gAAAA{}", "H".repeat(100));
    let text = "Historical reply.\n  Preserve 中文 and whitespace.\n";
    let recovery = history_upstream(vec![
        json!({"id":"resp_native_child","status":"completed","output":[{
            "type":"function_call","namespace":"collaboration","name":"send_message",
            "call_id":"child-report","arguments":json!({"target":"/root","message":cipher}).to_string()
        }],"usage":{"input_tokens":7,"output_tokens":5}}),
        json!({"id":"resp_capture","status":"completed","output":[{
            "type":"function_call","name":"capture_assignment",
            "arguments":json!({"text":text}).to_string()
        }],"usage":{"input_tokens":17,"output_tokens":9}}),
    ]);
    let external = history_upstream(vec![
        json!({
            "id":"chat_parent", "object":"chat.completion", "choices":[{
                "index":0,"message":{"role":"assistant","content":"parent continued"},"finish_reason":"stop"
            }]
        });
        5
    ]);
    let (_directory, server) = configured_server(&external.base_url());
    {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        config["providers"].as_array_mut().unwrap().push(json!({
            "id":"native-fixture","base_url":recovery.base_url(),"protocol":"responses","auth_mode":"forward"
        }));
        config["models"].as_array_mut().unwrap().push(json!({
            "id":"native-fixture/gpt","provider":"native-fixture","upstream_id":"gpt-fixture","enabled":true
        }));
    }
    let service = &server.state.backend.transport.collaboration;
    let origin = |cipher: &str| {
        json!({"type":"response.output_item.done","item":{
            "type":"function_call","namespace":"collaboration","name":"send_message",
            "arguments":json!({"target":"/root","message":cipher}).to_string()
        }})
    };
    let agent = |cipher: &str| {
        json!({"type":"agent_message","author":"/root/worker","recipient":"/root","content":[
            {"type":"input_text","text":"Message Type: MESSAGE\nTask name: /root\nSender: /root/worker\nPayload:\n"},
            {"type":"encrypted_content","encrypted_content":cipher}
        ]})
    };
    let request = json!({"model":"demo/model","stream":false,"input":[
        {"role":"user","content":"earlier task"}, agent(&cipher),
        {"type":"function_call","call_id":"history-call","name":"read","arguments":"{}"},
        {"type":"function_call_output","call_id":"history-call","output":"exact result\n"},
        {"role":"user","content":"continue here\n"}
    ],"tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]});
    let session = session_header(&server);
    let headers = [
        &*session,
        "Authorization: Bearer native-fixture",
        "chatgpt-account-id: native-owner",
    ];
    // A native GPT child can receive a readable assignment from an external
    // parent even with external children disabled. Observe its encrypted reply
    // through the real native response path, rather than seeding provenance.
    let child_input = json!({"model":"native-fixture/gpt","stream":false,"input":[{
        "type":"agent_message","author":"/root","recipient":"/root/worker",
        "content":[{"type":"input_text","text":"readable external parent assignment"}]
    }]});
    let child_response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&child_input).unwrap(),
        &[
            &*session,
            "Authorization: Bearer native-fixture",
            "chatgpt-account-id: native-owner",
            "x-openai-subagent: collab_spawn",
        ],
    );
    assert!(
        child_response.starts_with("HTTP/1.1 200"),
        "{child_response}"
    );
    let (_, _, child_upstream) = recovery.observed();
    assert_eq!(child_upstream["model"], "gpt-fixture");
    assert_eq!(child_upstream["input"], child_input["input"]);
    let send = |body: &Value| {
        post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(body).unwrap(),
            &headers,
        )
    };
    // Ordinary child-completion notifications are readable, so an external
    // parent can consume them without enabling or charging recovery.
    let plain = json!({"model":"demo/model","stream":false,"input":[{
        "type":"agent_message","author":"/root/worker","recipient":"/root",
        "content":[{"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\nreadable child result"}]
    }]});
    let plain_response = send(&plain);
    assert!(
        plain_response.starts_with("HTTP/1.1 200"),
        "{plain_response}"
    );
    let (_, _, plain_forwarded) = external.observed();
    assert!(
        plain_forwarded["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("readable child result")
    );
    assert!(
        recovery.observed.try_recv().is_err(),
        "readable results need no recovery inference"
    );
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["external_collaboration"] = json!("on_demand");
    let send_child = |body: &Value| {
        post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(body).unwrap(),
            &[
                &*session,
                "Authorization: Bearer native-fixture",
                "chatgpt-account-id: native-owner",
                "x-openai-subagent: collab_spawn",
            ],
        )
    };
    let mut unknown = request.clone();
    unknown["input"]
        .as_array_mut()
        .unwrap()
        .push(agent(&format!("gAAAA{}", "U".repeat(100))));
    let other = format!("gAAAA{}", "O".repeat(100));
    service.observe("native-fixture/gpt", "another-owner", &origin(&other));
    let mut mixed = request.clone();
    mixed["input"].as_array_mut().unwrap().push(agent(&other));
    let mut malformed = request.clone();
    let mut invalid = agent(&cipher);
    invalid["content"][0]["text"] = json!("invalid header");
    malformed["input"].as_array_mut().unwrap().push(invalid);
    let mut oversized = request.clone();
    oversized["input"] = json!(vec![agent(&cipher); 9]);
    for (body, code) in [
        (&unknown, "agent_task_origin_unknown"),
        (&mixed, "agent_task_account_changed"),
        (&malformed, "invalid_agent_task"),
        (&oversized, "agent_task_recovery_limit"),
    ] {
        let rejected = send_child(body);
        assert!(rejected.starts_with("HTTP/1.1 400"), "{rejected}");
        assert!(rejected.contains(code), "{rejected}");
    }
    // A root turn can inherit outbound tasks as well as inbound replies.
    let mut inherited = request.clone();
    inherited["input"].as_array_mut().unwrap().insert(
        1,
        json!({
            "type":"agent_message", "author":"/root", "recipient":"/root/old-worker",
            "content":[{"type":"input_text","text":"old delegation"}]
        }),
    );
    for mode in ["off", "on"] {
        {
            let mut config = server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap();
            config["external_collaboration"] = json!(mode);
        }
        for body in [&request, &inherited] {
            let response = send(body);
            assert!(response.starts_with("HTTP/1.1 400"), "{mode}: {response}");
            assert!(
                response.contains("encrypted_agent_task_requires_recovery"),
                "{response}"
            );
            assert!(
                !response.contains("Settings > Recover encrypted history"),
                "{response}"
            );
        }
        let mut socket = emp_transport::ClientWebSocket::connect(
            &format!("ws://{}/v1/responses", server.local_addr()),
            &BTreeMap::from([
                ("X-EMP-Session".into(), server.session_token().to_owned()),
                ("Authorization".into(), "Bearer native-fixture".into()),
                ("chatgpt-account-id".into(), "native-owner".into()),
            ]),
            Duration::from_secs(5),
        )
        .unwrap();
        let mut ws = inherited.clone();
        ws["type"] = json!("response.create");
        ws["stream"] = json!(true);
        socket.send_json(&ws).unwrap();
        let mut event = socket.receive_json().unwrap().unwrap();
        if event["type"] == "codex.response.metadata" {
            event = socket.receive_json().unwrap().unwrap();
        }
        assert_eq!(event["type"], "error", "{event}");
        assert_eq!(event["status"], 400, "{event}");
        assert_eq!(
            event["error"]["code"], "encrypted_agent_task_requires_recovery",
            "{event}"
        );
        socket.close();
    }
    assert!(
        recovery.observed.try_recv().is_err(),
        "disabled modes must not spend quota"
    );
    assert!(
        external.observed.try_recv().is_err(),
        "disabled modes must not forward opaque history"
    );
    {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        config["external_collaboration"] = json!("on_demand");
    }
    let source_before = serde_json::to_vec(&request).unwrap();
    let snapshot_file = _directory
        .path()
        .join("codex/sessions/rollout-synthetic.jsonl");
    fs::create_dir_all(snapshot_file.parent().unwrap()).unwrap();
    let snapshot_before =
        b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic-closed-thread\"}}\n";
    fs::write(&snapshot_file, snapshot_before).unwrap();
    for (request, index) in [(&request, 1), (&inherited, 2)] {
        let response = send(request);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let (_, _, projected) = external.observed();
        assert_eq!(
            projected["messages"][index]["content"],
            format!(
                "Message Type: MESSAGE\nTask name: /root\nSender: /root/worker\nPayload:\n{text}"
            )
        );
        assert_eq!(
            projected["messages"][index + 1]["tool_calls"][0]["id"],
            "history-call"
        );
        assert_eq!(
            projected["messages"][index + 2]["tool_call_id"],
            "history-call"
        );
        assert_eq!(
            projected["messages"][index + 2]["content"],
            "exact result\n"
        );
        assert_eq!(
            projected["messages"][index + 3]["content"],
            "continue here\n"
        );
        assert_eq!(projected["tools"][0]["function"]["name"], "read");
        assert!(!projected.to_string().contains(&cipher));

        let mut socket = emp_transport::ClientWebSocket::connect(
            &format!("ws://{}/v1/responses", server.local_addr()),
            &BTreeMap::from([
                ("X-EMP-Session".into(), server.session_token().to_owned()),
                ("Authorization".into(), "Bearer native-fixture".into()),
                ("chatgpt-account-id".into(), "native-owner".into()),
            ]),
            Duration::from_secs(5),
        )
        .unwrap();
        let mut ws = request.clone();
        ws["type"] = json!("response.create");
        ws["stream"] = json!(true);
        socket.send_json(&ws).unwrap();
        loop {
            let event = socket.receive_json().unwrap().unwrap();
            assert!(
                !matches!(event["type"].as_str(), Some("error" | "response.failed")),
                "{event}"
            );
            if event["type"] == "response.completed" {
                break;
            }
        }
        socket.close();
        let (_, _, replayed) = external.observed();
        assert_eq!(replayed["messages"], projected["messages"]);
    }
    let (_, _, captured) = recovery.observed();
    assert_eq!(captured["model"], "gpt-6-luna");
    assert_eq!(captured["reasoning"]["effort"], "low");
    assert_eq!(captured["input"], json!([agent(&cipher)]));
    assert!(
        recovery.observed.try_recv().is_err(),
        "all replays must reuse the one recovery"
    );
    assert_eq!(serde_json::to_vec(&request).unwrap(), source_before);
    assert_eq!(fs::read(&snapshot_file).unwrap(), snapshot_before);
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    assert_eq!(
        snapshot["requests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["route"] == "agent_task_recovery")
            .count(),
        1,
        "HTTP/WS replays must account exactly one helper inference"
    );
    server.shutdown().unwrap();
}
