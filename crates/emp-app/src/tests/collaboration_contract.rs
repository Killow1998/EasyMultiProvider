use super::*;

#[test]
fn collaboration_websocket_rejects_disabled_and_unowned_tasks_without_forwarding() {
    let (_directory, server) = configured_server("http://127.0.0.1:1/v1");
    let url = format!("ws://{}/v1/responses", server.local_addr());
    let headers = BTreeMap::from([
        ("X-EMP-Session".into(), server.session_token().to_owned()),
        ("x-openai-subagent".into(), "collab_spawn".into()),
    ]);
    let mut socket = emp_test_support::websocket::ClientWebSocket::connect(
        &url,
        &headers,
        Duration::from_secs(5),
    )
    .unwrap();
    let task = json!({"type":"agent_message","author":"/root","recipient":"/root/bench","content":[
        {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/bench\nSender: /root\nPayload:\n"},
        {"type":"encrypted_content","encrypted_content":format!("gAAAA{}", "A".repeat(100))}
    ]});
    for (mode, code) in [
        ("off", "external_subagents_disabled"),
        ("on", "encrypted_agent_task_requires_recovery"),
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
        assert_eq!(event["status"], 400, "{event}");
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
