//! Root history recovery is a paid opt-in, independent of fresh child routing.
use super::*;

#[test]
fn parent_history_recovery_preserves_tools_and_input_on_http_and_websocket() {
    let cipher = format!("gAAAA{}", "H".repeat(100));
    let text = "Historical reply.\n  Preserve 中文 and whitespace.\n";
    let recovery = OneShotUpstream::start_wire(
        200,
        "application/json",
        None,
        vec![
            serde_json::to_vec(&json!({"id":"resp_capture","status":"completed","output":[{
            "type":"function_call","name":"capture_assignment",
            "arguments":json!({"text":text}).to_string()
        }],"usage":{"input_tokens":17,"output_tokens":9}}))
            .unwrap(),
        ],
    );
    let external = OneShotUpstream::start_repeated_wire(200, "application/json", None, 2, vec![
        serde_json::to_vec(&json!({"id":"chat_parent","object":"chat.completion","choices":[{
            "index":0,"message":{"role":"assistant","content":"parent continued"},"finish_reason":"stop"
        }]})).unwrap()
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
    let native_owner = emp_state::usage::account_owner(&BTreeMap::from([
        ("authorization".into(), "Bearer native-fixture".into()),
        ("chatgpt-account-id".into(), "native-owner".into()),
    ]));
    service.observe("native-fixture/gpt", &native_owner, &origin(&cipher));
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
    let send = |body: &Value| {
        post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(body).unwrap(),
            &headers,
        )
    };
    let disabled = send(&request);
    assert!(disabled.starts_with("HTTP/1.1 409"), "{disabled}");
    assert!(
        disabled.contains("Recover encrypted history") && disabled.contains("readable handoff")
    );
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["encrypted_history_recovery"] = json!(true);
    let child = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&request).unwrap(),
        &[&session, "x-openai-subagent: collab_spawn"],
    );
    assert!(child.starts_with("HTTP/1.1 403"), "{child}");
    let mut unknown = request.clone();
    unknown["input"]
        .as_array_mut()
        .unwrap()
        .push(agent(&format!("gAAAA{}", "U".repeat(100))));
    let unknown_result = send(&unknown);
    assert!(
        unknown_result.contains("agent_task_origin_unknown"),
        "{unknown_result}"
    );
    let other = format!("gAAAA{}", "O".repeat(100));
    service.observe("native-fixture/gpt", "another-owner", &origin(&other));
    let mut mixed = request.clone();
    mixed["input"].as_array_mut().unwrap().push(agent(&other));
    let mixed_result = send(&mixed);
    assert!(
        mixed_result.contains("agent_task_account_changed"),
        "{mixed_result}"
    );
    let mut malformed = request.clone();
    let mut invalid = agent(&cipher);
    invalid["content"][0]["text"] = json!("invalid header");
    malformed["input"].as_array_mut().unwrap().push(invalid);
    assert!(send(&malformed).contains("invalid_agent_task"));
    let mut oversized = request.clone();
    oversized["input"] = json!(vec![agent(&cipher); 9]);
    assert!(send(&oversized).contains("agent_task_recovery_limit"));
    assert!(
        recovery.observed.try_recv().is_err(),
        "preflight rejection must not spend quota"
    );
    assert!(
        external.observed.try_recv().is_err(),
        "no partially recovered history may be forwarded"
    );
    let review = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&request).unwrap(),
        &[&session, "x-openai-subagent: review"],
    );
    assert!(
        review.contains("encrypted_agent_task_requires_recovery"),
        "root-only opt-in cannot charge review workers: {review}"
    );

    let response = send(&request);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let (_, _, captured) = recovery.observed();
    assert_eq!(captured["model"], "gpt-6-luna");
    assert_eq!(captured["reasoning"]["effort"], "low");
    assert_eq!(captured["input"], json!([agent(&cipher)]));
    let (_, _, projected) = external.observed();
    assert_eq!(
        projected["messages"][1]["content"],
        format!("Message Type: MESSAGE\nTask name: /root\nSender: /root/worker\nPayload:\n{text}")
    );
    assert_eq!(
        projected["messages"][2]["tool_calls"][0]["id"],
        "history-call"
    );
    assert_eq!(projected["messages"][3]["tool_call_id"], "history-call");
    assert_eq!(projected["messages"][3]["content"], "exact result\n");
    assert_eq!(projected["messages"][4]["content"], "continue here\n");
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
    assert_eq!(
        request["input"][1],
        agent(&cipher),
        "source request must remain untouched"
    );
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
        "HTTP + WS cache replay must use exactly one accounted helper inference"
    );
    server.shutdown().unwrap();
}
