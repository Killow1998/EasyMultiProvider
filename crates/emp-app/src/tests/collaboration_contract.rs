//! Native parent -> validated recovery -> external child, including retry and replay.
use super::*;

#[test]
fn collaboration_recovery_rejects_invalid_capture_and_accounts_retries_once() {
    let ciphertext = format!("gAAAA{}", "A".repeat(100));
    let assignment = "Review only.\n  Preserve whitespace and 中文.\nDo not run commands.";
    let parent = json!({"id":"resp_parent","status":"completed","output":[{
        "type":"function_call","id":"fc_parent","call_id":"call_parent","namespace":"collaboration",
        "name":"send_message","arguments":json!({"target":"/root/worker","message":ciphertext}).to_string()
    }],"usage":{"input_tokens":10,"output_tokens":5}});
    let recovered = json!({"id":"resp_recovery","status":"completed","output":[{
        "type":"function_call","id":"fc_recovery","call_id":"call_recovery","name":"capture_assignment",
        "arguments":json!({"text":assignment}).to_string()
    }],"usage":{"input_tokens":21,"output_tokens":13}});
    let mut invalid = recovered.clone();
    invalid["id"] = json!("resp_recovery_invalid");
    invalid["output"][0]["arguments"] = json!("{}");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let native_base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (sender, native_requests) = mpsc::channel();
    let worker = thread::spawn(move || {
        for response in [parent, invalid, recovered] {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(Instant::now() < deadline, "missing Native request");
                thread::sleep(Duration::from_millis(5));
            };
            sender.send(receive_upstream_request(&mut stream)).unwrap();
            let bytes = serde_json::to_vec(&response).unwrap();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
            stream.write_all(&bytes).unwrap();
        }
    });
    let answer = json!({"id":"chat_child","object":"chat.completion","model":"external-gpt",
        "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}});
    let external = OneShotUpstream::start_repeated_wire(
        200,
        "application/json",
        None,
        2,
        vec![serde_json::to_vec(&answer).unwrap()],
    );
    let (_directory, server) = configured_server(&external.base_url());
    {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        config["external_collaboration"] = json!("on_demand");
        config["providers"].as_array_mut().unwrap().push(json!({"id":"native-fixture","base_url":native_base,"protocol":"responses","auth_mode":"forward"}));
        config["models"].as_array_mut().unwrap().push(json!({"id":"native-fixture/gpt","provider":"native-fixture","upstream_id":"gpt-fixture","enabled":true}));
    }
    let session = session_header(&server);
    let headers = [
        &*session,
        "Authorization: Bearer native-fixture",
        "chatgpt-account-id: native-owner",
    ];
    let result = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(
            &json!({"model":"native-fixture/gpt","input":"delegate","stream":false}),
        )
        .unwrap(),
        &headers,
    );
    assert!(result.starts_with("HTTP/1.1 200"), "{result}");
    let agent = json!({"type":"agent_message","author":"/root","recipient":"/root/worker","content":[
        {"type":"input_text","text":"Message Type: MESSAGE\nTask name: /root/worker\nSender: /root\nPayload:\n"},
        {"type":"encrypted_content","encrypted_content":ciphertext}
    ]});
    let body =
        serde_json::to_vec(&json!({"model":"demo/model","input":[agent],"stream":false})).unwrap();
    let rejected = post(&server, "/v1/responses", &body, &headers);
    assert!(rejected.starts_with("HTTP/1.1 502"), "{rejected}");
    assert!(rejected.contains("agent_task_recovery_invalid"));
    for _ in 0..2 {
        let response = post(&server, "/v1/responses", &body, &headers);
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let (_, external_headers, projected) = external.observed();
        assert_eq!(external_headers["authorization"], "Bearer upstream-secret");
        let encoded = projected.to_string();
        assert!(!encoded.contains(&ciphertext));
        assert!(encoded.contains("Preserve whitespace"));
        assert_eq!(
            projected["messages"][0]["content"],
            format!(
                "Message Type: MESSAGE\nTask name: /root/worker\nSender: /root\nPayload:\n{assignment}"
            )
        );
    }
    worker.join().unwrap();
    let requests: Vec<_> = native_requests.try_iter().collect();
    assert_eq!(
        requests.len(),
        3,
        "failed capture is retried; successful capture is reused"
    );
    for request in &requests[1..] {
        assert_eq!(request.1["authorization"], "Bearer native-fixture");
        assert_eq!(request.2["model"], "gpt-6-luna");
        assert_eq!(request.2["reasoning"]["effort"], "low");
        assert_eq!(request.2["input"].as_array().unwrap().len(), 1);
    }
    let mismatch = post(
        &server,
        "/v1/responses",
        &body,
        &[
            &session,
            "Authorization: Bearer changed",
            "chatgpt-account-id: another-owner",
        ],
    );
    assert!(
        mismatch.contains("agent_task_account_changed"),
        "{mismatch}"
    );
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["external_collaboration"] = json!("off");
    let blocked = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&json!({"model":"demo/model","input":"plain task"})).unwrap(),
        &[&session, "x-openai-subagent: collab_spawn"],
    );
    assert!(blocked.starts_with("HTTP/1.1 403"), "{blocked}");
    assert!(
        blocked.contains("external_subagents_disabled")
            && blocked.contains("Settings > Cross-service subagents")
    );
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    let recoveries: Vec<_> = snapshot["requests"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["route"] == "agent_task_recovery")
        .collect();
    assert_eq!(recoveries.len(), 2);
    assert_eq!(
        recoveries
            .iter()
            .filter(|row| row["state"] == "failed")
            .count(),
        1
    );
    for recovery in &recoveries {
        assert_eq!(recovery["input_tokens"], 21);
        assert_eq!(recovery["output_tokens"], 13);
    }
    let now = crate::util::system_now();
    let totals = server
        .state
        .backend
        .usage
        .ledger
        .query(now - 60.0, now + 1.0, "all", now)
        .unwrap();
    let recovery = totals["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|group| group["model"] == "gpt-6-luna")
        .unwrap();
    assert_eq!(recovery["requests"], 2);
    assert_eq!(recovery["input_tokens"], 42);
    assert_eq!(recovery["output_tokens"], 26);
    server.shutdown().unwrap();
}

#[test]
fn collaboration_websocket_rejects_disabled_and_unowned_tasks_without_forwarding() {
    let (_directory, server) = configured_server("http://127.0.0.1:1/v1");
    let url = format!("ws://{}/v1/responses", server.local_addr());
    let headers = BTreeMap::from([("X-EMP-Session".into(), server.session_token().to_owned())]);
    let mut socket =
        emp_transport::ClientWebSocket::connect(&url, &headers, Duration::from_secs(5)).unwrap();
    let task = json!({"type":"agent_message","author":"/root","recipient":"/root/bench","content":[
        {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/bench\nSender: /root\nPayload:\n"},
        {"type":"encrypted_content","encrypted_content":format!("gAAAA{}", "A".repeat(100))}
    ]});
    for (mode, code) in [
        ("off", "external_subagents_disabled"),
        ("on_demand", "agent_task_origin_unknown"),
    ] {
        server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap()["external_collaboration"] = json!(mode);
        socket
            .send_json(&json!({"type":"response.create","model":"demo/model","input":[task]}))
            .unwrap();
        let mut event = socket.receive_json().unwrap().unwrap();
        if event["type"] == "codex.response.metadata" {
            event = socket.receive_json().unwrap().unwrap();
        }
        assert_eq!(event["type"], "error", "{event}");
        assert_eq!(event["error"]["code"], code, "{event}");
        if mode == "off" {
            assert!(
                event["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("Settings > Cross-service subagents")
            );
        }
    }
    socket.close();
    server.shutdown().unwrap();
}
