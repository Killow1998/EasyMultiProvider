//! Real API rejection, wire compatibility, request correlation and no dispatch.
use super::request_observation_contract::finished;
use super::*;

fn next_response(socket: &mut emp_transport::ClientWebSocket) -> Value {
    loop {
        let event = socket.receive_json().unwrap().unwrap();
        if event["type"] != "codex.response.metadata" {
            return event;
        }
    }
}

fn assert_failure(records: &[Value], detail: &Value, category: &str, reason: &str) {
    assert_eq!(detail["error_class"], "history_reconstruction_failed");
    assert_eq!(detail["category"], category);
    assert_eq!(detail["reason"], reason);
    assert!(detail["message"].as_str().unwrap().contains(reason));
    let done = records
        .iter()
        .rev()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    let failures: Vec<_> = records
        .iter()
        .filter(|r| {
            r["event"] == "history_reconstruction_failed"
                && r["fields"]["request_id"] == done["fields"]["request_id"]
        })
        .collect();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0]["fields"]["reason"], reason);
    assert_eq!(
        done["fields"]["history_failure"],
        json!({"phase":"prepare_history", "category":category, "reason":reason})
    );
    assert_eq!(done["fields"]["delivery"], "terminal_written");
    assert!(
        !records
            .iter()
            .any(|r| r["event"] == "model_attempt_started")
    );
    assert!(!serde_json::to_string(records).unwrap().contains("private-"));
}

#[test]
fn history_failures_match_across_http_compact_sse_and_websocket() {
    let (root, server) = configured_server("http://127.0.0.1:1/v1");
    let session = session_header(&server);
    let headers = BTreeMap::from([(
        "x-emp-session".into(),
        session.trim_start_matches("X-EMP-Session: ").into(),
    )]);
    let mut socket = emp_transport::ClientWebSocket::connect(
        &format!("ws://{}/v1/responses", server.local_addr()),
        &headers,
        Duration::from_secs(3),
    )
    .unwrap();
    let mut count = 0;
    for (mut body, category, reason) in [
        (
            json!({"input":[{"type":"compaction", "encrypted_content":"private-checkpoint"}]}),
            "anchor",
            "thread_identity_missing",
        ),
        (
            json!({"input":[{"type":"compaction", "encrypted_content":"emp1:private-invalid"}]}),
            "checkpoint",
            "portable_checkpoint_encoding_invalid",
        ),
        (
            json!({"input":[{"type":"compaction", "encrypted_content":"private-checkpoint"}],
            "client_metadata":{"x-codex-turn-metadata":"private-invalid-json"}}),
            "anchor",
            "invalid_turn_metadata",
        ),
    ] {
        body["model"] = json!("demo/model");
        for path in ["/v1/responses", "/v1/responses/compact"] {
            let reply = post(
                &server,
                path,
                &serde_json::to_vec(&body).unwrap(),
                &[&session],
            );
            assert!(reply.starts_with("HTTP/1.1 409"), "{reply}");
            let response: Value =
                serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(response["error"]["code"], "history_reconstruction_failed");
            count += 1;
            assert_failure(
                &finished(root.path(), count),
                &response["error"],
                category,
                reason,
            );
        }
        body["stream"] = json!(true);
        let reply = post_stream(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &[&session],
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(reply.contains("event: response.failed"), "{reply}");
        let failed: Value = serde_json::from_str(
            reply
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(failed["response"]["error"]["code"], "invalid_prompt");
        count += 1;
        assert_failure(
            &finished(root.path(), count),
            &failed["response"]["error"],
            category,
            reason,
        );

        body["type"] = json!("response.create");
        socket.send_json(&body).unwrap();
        let failed = next_response(&mut socket);
        assert_eq!(failed["type"], "response.failed");
        assert_eq!(failed["response"]["error"]["code"], "invalid_prompt");
        count += 1;
        assert_failure(
            &finished(root.path(), count),
            &failed["response"]["error"],
            category,
            reason,
        );
    }
    // A normal full-request fallback on the same connection must not inherit
    // the previous turn's failure or be classified as lost local history.
    socket.send_json(&json!({"type":"response.create", "model":"demo/model", "previous_response_id":"private-prior", "input":[]})).unwrap();
    let fallback = next_response(&mut socket);
    assert_eq!(fallback["error"]["code"], "previous_response_not_found");
    let records = finished(root.path(), count + 1);
    let done = records
        .iter()
        .rev()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert!(done["fields"]["history_failure"].is_null());
    assert_eq!(
        records
            .iter()
            .filter(|r| r["event"] == "history_reconstruction_failed")
            .count(),
        count
    );
    assert!(
        !records
            .iter()
            .any(|r| r["event"] == "model_attempt_started")
    );
    drop(socket);
    server.shutdown().unwrap();
}

#[test]
fn missing_summary_output_is_distinct_from_history_lookup_and_does_not_retry() {
    let upstream = OneShotUpstream::start(json!({"id":"fixture", "choices":[{"index":0,
        "message":{"role":"assistant","content":""}, "finish_reason":"stop"}]}));
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    std::fs::write(&config, json!({
        "providers":[{"id":"short", "base_url":upstream.base_url(), "protocol":"chat_completions",
            "auth_mode":"api_key", "api_key":"fixture-key"}],
        "models":[{"id":"short/model", "provider":"short", "upstream_id":"short-model", "enabled":true,
            "context_window":1200, "output_limit":64,
            "capability_sources":{"context_window":{"source":"manual", "confidence":1.0}}}]
    }).to_string()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-test-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let mut input = vec![json!({"role":"user", "content":"x".repeat(500)}); 4];
    input.push(json!({"role":"user", "content":"private-active-request"}));
    let body = json!({"model":"short/model", "max_output_tokens":64, "input":input});
    let response = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body).unwrap(),
        &[&session_header(&server)],
    );
    assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["error"]["reason"], "summary_output_missing");
    assert_eq!(body["error"]["category"], "destination_compaction");
    let (_, _, sent) = upstream.observed();
    assert!(!sent.to_string().contains("private-active-request"));
    let records = finished(directory.path(), 1);
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(
        done["fields"]["history_failure"],
        json!({"phase":"prepare_destination", "category":"destination_compaction", "reason":"summary_output_missing"})
    );
    let attempts: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "model_attempt_started")
        .collect();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0]["fields"]["request_id"],
        done["fields"]["request_id"]
    );
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-active-request")
    );
    server.shutdown().unwrap();
}
