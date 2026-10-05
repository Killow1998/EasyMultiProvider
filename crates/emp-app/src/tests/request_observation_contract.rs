//! Loopback acceptance: receipts explain execution without controlling it.
use super::internal_events_contract::journal;
use super::*;

#[test]
fn cancelled_compaction_does_not_claim_a_terminal_write() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted, ready) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        receive_upstream_request(&mut stream);
        accepted.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut byte = [0];
        matches!(stream.read(&mut byte), Ok(0))
    });
    let (root, server) = configured_server(&format!("http://{address}/v1"));
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","input":[{"role":"user","content":"synthetic input"},{"type":"compaction_trigger"}],"stream":true}"#,
        &[&session_header(&server)],
    );
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    drop(downstream);
    let upstream_closed = worker.join().unwrap();
    let records = finished(root.path(), 1);
    server.shutdown().unwrap();
    assert!(upstream_closed);
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(done["fields"]["terminal_written"], false, "{done}");
    assert_eq!(done["fields"]["writes_completed"], 0, "{done}");
    assert_eq!(done["fields"]["transport"], "sse", "{done}");
    assert_eq!(done["fields"]["delivery"], "not_observed", "{done}");
    let http = records
        .iter()
        .find(|r| r["event"] == "http_request_completed")
        .unwrap();
    assert_eq!(http["fields"]["result"], "no_response", "{http}");
    assert!(http["fields"]["status"].is_null(), "{http}");
}

#[test]
fn history_sse_failure_is_recorded_as_failed() {
    let (root, server) = configured_server("http://127.0.0.1:1/v1");
    let reply = post_stream(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","stream":true,"input":[{"type":"compaction","encrypted_content":"opaque-fixture"}]}"#,
        &[&session_header(&server)],
    );
    let records = finished(root.path(), 1);
    server.shutdown().unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("event: response.failed"), "{reply}");
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(done["fields"]["delivery"], "terminal_written");
    assert_eq!(done["fields"]["downstream_terminal"], "failed", "{done}");
}

#[test]
fn generated_compaction_sse_records_its_actual_terminal_event() {
    let upstream = OneShotUpstream::start(chat_answer());
    let (root, server) = configured_server(&upstream.base_url());
    let reply = post_stream(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","input":[{"role":"user","content":"synthetic input"},{"type":"compaction_trigger"}],"stream":true}"#,
        &[&session_header(&server)],
    );
    let _ = upstream.observed();
    let records = finished(root.path(), 1);
    server.shutdown().unwrap();
    assert!(reply.contains("event: response.completed"), "{reply}");
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(done["fields"]["delivery"], "terminal_written");
    assert_eq!(done["fields"]["downstream_terminal"], "completed", "{done}");
}

fn finished(root: &Path, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let records = journal(root);
        if records
            .iter()
            .filter(|r| r["event"] == "request_finished")
            .count()
            == count
        {
            return records;
        }
        assert!(Instant::now() < deadline, "missing request receipts");
        thread::sleep(Duration::from_millis(5));
    }
}

fn chat_answer() -> Value {
    json!({"id":"chat_fixture","model":"upstream-model","choices":[{"index":0,
        "message":{"role":"assistant","content":"private-answer"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
}

#[test]
fn http_and_compact_use_one_id_from_entry_through_dispatch_and_write() {
    for path in ["/v1/responses", "/v1/responses/compact"] {
        let upstream = OneShotUpstream::start(chat_answer());
        let (root, server) = configured_server(&upstream.base_url());
        let body = json!({"model":"demo/model","input":"private-question","reasoning":{"effort":"medium"}});
        let reply = post(
            &server,
            path,
            &serde_json::to_vec(&body).unwrap(),
            &[
                &session_header(&server),
                "x-emp-request-id: private-spoof",
                "X-EMP-Request-ID: aaaaaaaaaaaaaaaa",
            ],
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        let (_, headers, sent) = upstream.observed();
        let id = &headers["x-emp-request-id"];
        assert_ne!(id, "aaaaaaaaaaaaaaaa");
        assert_eq!(id.len(), 16);
        if path == "/v1/responses" {
            assert_eq!(sent["reasoning_effort"], "medium");
            assert!(reply.contains("private-answer"));
        }
        let records = finished(root.path(), 1);
        assert_eq!(
            records
                .iter()
                .filter(|r| r["event"] == "model_attempt_started")
                .count(),
            1
        );
        for event in [
            "http_request_started",
            "request_started",
            "request_route_selected",
            "route_observation",
            "request_finished",
        ] {
            assert!(
                records
                    .iter()
                    .any(|r| r["event"] == event && r["fields"]["request_id"] == *id),
                "missing {event}"
            );
        }
        let done = records
            .iter()
            .find(|r| r["event"] == "request_finished")
            .unwrap();
        assert_eq!(done["fields"]["delivery"], "terminal_written");
        assert_eq!(done["fields"]["response_status"], 200);
        assert_eq!(done["fields"]["requested_effort"], "medium");
        assert_eq!(done["fields"]["provider_id"], "demo");
        assert_eq!(done["fields"]["last_phase"], "execute_and_relay");
        let text = serde_json::to_string(&records).unwrap();
        for private in [
            "private-question",
            "private-answer",
            "private-spoof",
            "upstream-secret",
        ] {
            assert!(!text.contains(private), "disclosed {private}");
        }
        server.shutdown().unwrap();
    }
}

#[test]
fn sse_receipts_distinguish_model_completion_and_written_failure() {
    for completed in [true, false] {
        let mut events = vec![
            json!({"id":"chat_fixture","choices":[{"index":0,"delta":{"content":"answer"},"finish_reason":null}]}),
        ];
        if completed {
            events.push(json!({"id":"chat_fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
        }
        let mut wire = events
            .iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>();
        if completed {
            wire.push_str("data: [DONE]\n\n");
        }
        let upstream = OneShotUpstream::start_sse(vec![wire.into_bytes()]);
        let (root, server) = configured_server(&upstream.base_url());
        let reply = post_stream(
            &server,
            "/v1/responses",
            br#"{"model":"demo/model","input":"hello","stream":true}"#,
            &[&session_header(&server)],
        );
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        let (_, headers, _) = upstream.observed();
        let records = finished(root.path(), 1);
        let done = records
            .iter()
            .find(|r| r["event"] == "request_finished")
            .unwrap();
        assert_eq!(done["fields"]["request_id"], headers["x-emp-request-id"]);
        assert_eq!(done["fields"]["transport"], "sse");
        assert_eq!(
            done["fields"]["downstream_terminal"],
            if completed { "completed" } else { "failed" }
        );
        // Both a successful response and a projected error can be written fully.
        assert_eq!(done["fields"]["delivery"], "terminal_written");
        assert_eq!(
            records
                .iter()
                .filter(|r| r["event"] == "model_attempt_started")
                .count(),
            1
        );
        server.shutdown().unwrap();
    }
}

#[test]
fn a_stream_request_rejected_before_output_records_the_buffered_error_transport() {
    let upstream = OneShotUpstream::start_wire(
        401,
        "application/json",
        None,
        vec![br#"{"error":{"type":"auth","message":"private-upstream-failure"}}"#.to_vec()],
    );
    let (root, server) = configured_server(&upstream.base_url());
    let reply = post(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","input":"hello","stream":true}"#,
        &[&session_header(&server)],
    );
    assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    upstream.observed();
    let records = finished(root.path(), 1);
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(done["fields"]["requested_transport"], "sse");
    assert_eq!(done["fields"]["transport"], "http");
    assert_eq!(done["fields"]["response_status"], 401);
    assert_eq!(done["fields"]["delivery"], "terminal_written");
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-upstream-failure")
    );
    server.shutdown().unwrap();
}

#[test]
fn early_http_rejections_have_receipts_without_dispatch() {
    let (root, server) = test_server();
    assert!(post(&server, "/v1/responses", b"{}", &[]).starts_with("HTTP/1.1 401"));
    assert!(
        post(
            &server,
            "/v1/responses",
            b"private-invalid-json",
            &[&session_header(&server)]
        )
        .starts_with("HTTP/1.1 400")
    );
    let records = finished(root.path(), 2);
    assert!(
        !records
            .iter()
            .any(|r| r["event"] == "model_attempt_started")
    );
    let done: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "request_finished")
        .collect();
    assert_eq!(done[0]["fields"]["last_phase"], "caller_access");
    assert_eq!(done[1]["fields"]["last_phase"], "read_body");
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-invalid-json")
    );
    server.shutdown().unwrap();
}

#[test]
fn websocket_rejection_and_success_have_separate_turn_ids_on_the_same_connection() {
    let mut wire = b"data: {\"id\":\"chat_fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chat_fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec();
    wire.extend_from_slice(b"data: [DONE]\n\n");
    let upstream = OneShotUpstream::start_sse(vec![wire]);
    let (root, server) = configured_server(&upstream.base_url());
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
    socket
        .send_json(&json!({"type":"private-invalid-type"}))
        .unwrap();
    assert_eq!(socket.receive_json().unwrap().unwrap()["type"], "error");
    socket.send_json(&json!({"type":"response.create","model":"demo/model","input":"private-question","reasoning":{"effort":"low"}})).unwrap();
    loop {
        let event = socket.receive_json().unwrap().unwrap();
        assert!(
            !matches!(
                event["type"].as_str(),
                Some("error" | "response.failed" | "response.incomplete")
            ),
            "unexpected failure: {event}"
        );
        if event["type"] == "response.completed" {
            break;
        }
    }
    let (_, sent_headers, _) = upstream.observed();
    let records = finished(root.path(), 2);
    let done: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "request_finished")
        .collect();
    assert_ne!(
        done[0]["fields"]["request_id"],
        done[1]["fields"]["request_id"]
    );
    assert_eq!(
        done[0]["fields"]["connection_id"],
        done[1]["fields"]["connection_id"]
    );
    assert!(done[0]["fields"]["connection_id"].as_str().is_some());
    assert_eq!(done[0]["fields"]["downstream_terminal"], "error");
    assert_eq!(
        done[1]["fields"]["request_id"],
        sent_headers["x-emp-request-id"]
    );
    assert_eq!(done[1]["fields"]["downstream_terminal"], "completed");
    assert_eq!(
        records
            .iter()
            .filter(|r| r["event"] == "model_attempt_started")
            .count(),
        1
    );
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-")
    );
    drop(socket);
    server.shutdown().unwrap();
}

#[test]
fn an_unavailable_journal_does_not_prevent_a_real_request_or_add_attempts() {
    let upstream = OneShotUpstream::start(chat_answer());
    let root = tempfile::tempdir().unwrap();
    let path = canonical_root(&root);
    std::fs::create_dir_all(path.join("state")).unwrap();
    std::fs::write(path.join("state/logs"), b"block journal directory").unwrap();
    let config = path.join("config.json");
    std::fs::write(&config, json!({"providers":[{"id":"demo","base_url":upstream.base_url(),"protocol":"chat_completions","auth_mode":"api_key","api_key":"fixture-key"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream-model","enabled":true}]}).to_string()).unwrap();
    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    let reply = post(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","input":"hello"}"#,
        &[&session_header(&server)],
    );
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.contains("private-answer"));
    upstream.observed();
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    assert_eq!(snapshot["requests"][0]["attempts"], 1);
    server.shutdown().unwrap();
}

#[test]
fn downstream_write_failure_does_not_rewrite_or_repeat_upstream_accounting() {
    use crate::services::observation::request::RequestObservation;
    use crate::services::request_outcome::RequestOutcome;
    use crate::services::request_preparation::{RequestOperation, prepare_request};
    let (root, server) = configured_server("http://127.0.0.1:1/v1");
    let prepared = prepare_request(
        &server.state,
        json!({"model":"demo/model","input":"hello"}),
        RequestOperation::Responses,
    )
    .ok()
    .unwrap();
    let mut receipt = RequestObservation::new(
        Arc::clone(&server.state.backend.diagnostics),
        Some("0123456789abcdef".into()),
        None,
        "sse",
        "responses",
    );
    let headers = receipt.headers(BTreeMap::new());
    let terminal = json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":2}}});
    {
        let mut upstream = RequestOutcome::new(
            &server.state,
            &prepared.route,
            &prepared.body,
            &headers,
            None,
            "responses",
        );
        upstream.observe(&terminal);
        let write: std::io::Result<()> = Err(std::io::ErrorKind::BrokenPipe.into());
        assert_eq!(
            receipt.event_written(&terminal, write).unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }
    drop(receipt);
    let records = finished(root.path(), 1);
    let outcomes: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "route_observation")
        .collect();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["fields"]["status"], 200);
    assert_eq!(outcomes[0]["fields"]["terminal_event_observed"], true);
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(
        done["fields"]["request_id"],
        outcomes[0]["fields"]["request_id"]
    );
    assert_eq!(done["fields"]["delivery"], "write_failed");
    assert_eq!(done["fields"]["terminal_written"], false);
    server.shutdown().unwrap();
}
