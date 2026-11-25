//! Loopback acceptance: receipts explain execution without controlling it.
use super::*;
use crate::services::request_outcome::RequestGuard;
use crate::services::request_preparation::{RequestOperation, prepare_request};

fn settlement_fixture() -> (TempDir, ServerHandle, rusqlite::Connection) {
    let (root, server) = configured_server("http://127.0.0.1:1/v1");
    let db = settlement_db(root.path(), &server);
    (root, server, db)
}

pub(super) fn settlement_db(root: &Path, server: &ServerHandle) -> rusqlite::Connection {
    server
        .state
        .backend
        .usage
        .ledger
        .query(0.0, 1.0, "all", 1.0, None)
        .unwrap();
    rusqlite::Connection::open(root.join("state/usage.sqlite3")).unwrap()
}

pub(super) fn lock_settlement_writes(db: &mut rusqlite::Connection) -> rusqlite::Transaction<'_> {
    db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap()
}

fn settlement_outcome<'a>(
    server: &'a ServerHandle,
    operation: &str,
    request_id: &str,
) -> RequestGuard<'a> {
    let headers = BTreeMap::from([("x-emp-request-id".into(), request_id.into())]);
    let prepared = prepare_request(
        &server.state,
        json!({"model":"demo/model","input":"hello"}),
        RequestOperation::Responses,
        &headers,
    )
    .unwrap();
    RequestGuard::new(
        &server.state,
        &prepared.route,
        &prepared.body,
        &headers,
        None,
        operation,
    )
}

#[test]
fn receipt_failure_only_marks_usage_unavailable_for_billable_calls() {
    let (root, server, db) = settlement_fixture();
    db.execute_batch(
        "CREATE TRIGGER reject_receipt BEFORE INSERT ON call_records
         BEGIN SELECT RAISE(ABORT, 'fixture rejected receipt'); END;",
    )
    .unwrap();
    for (operation, request_id, billable) in [
        ("history_summary", "0123456789abcde1", false),
        ("responses", "0123456789abcde2", true),
    ] {
        let mut outcome = settlement_outcome(&server, operation, request_id);
        outcome.dispatch();
        outcome.observe(&json!({"type":"response.completed","response":{
            "status":"completed","usage":{"input_tokens":3,"output_tokens":2}
        }}));
        outcome.finish();

        let report = server
            .state
            .backend
            .usage
            .ledger
            .query(0.0, 1.0, "all", 1.0, None)
            .unwrap();
        assert_eq!(report["write_error"], billable, "{operation}");
        assert!(
            flushed_journal(root.path(), &server.state)
                .iter()
                .any(|record| {
                    record["event"] == "call_record_save_failed"
                        && record["fields"]["request_id"] == request_id
                }),
            "{operation}"
        );
    }
    server.shutdown().unwrap();
}

#[test]
fn route_diagnostics_are_visible_before_a_locked_receipt_can_commit() {
    let (_root, server, mut db) = settlement_fixture();
    for (operation, request_id, error, receipt_state) in [
        ("history_summary", "0123456789abcde1", "", "completed"),
        (
            "responses",
            "0123456789abcde2",
            "previous_response_not_found",
            "recovery_required",
        ),
    ] {
        let mut outcome = settlement_outcome(&server, operation, request_id);
        if error.is_empty() {
            outcome.http_status(200);
        } else {
            outcome.native_error(&emp_router::native_http::NativeHttpError {
                status: 400,
                body: json!({"error":{"origin":"emp","code":error}}),
                headers: BTreeMap::new(),
            });
        }
        thread::scope(|scope| {
            let lock = lock_settlement_writes(&mut db);
            let worker = scope.spawn(move || outcome.finish());
            wait_for(&Deadline::after(response_timeout()), || {
                server.state.backend.diagnostics.snapshot()["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|record| {
                        record["request_id"] == request_id
                            && record["route"] == operation
                            && record["error_code"] == error
                    })
            });
            let exists: bool = lock
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM call_records WHERE request_id=?)",
                    [request_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!exists, "{operation}");
            lock.commit().unwrap();
            worker.join().unwrap();
        });
        let state: String = db
            .query_row(
                "SELECT state FROM call_records WHERE request_id=? AND operation=?",
                [request_id, operation],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("missing {operation} receipt after unlocking: {error}"));
        assert_eq!(state, receipt_state, "{operation}");
    }
    server.shutdown().unwrap();
}

#[test]
fn cancelled_compaction_does_not_claim_a_terminal_write() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let observation = Observation::spawn(move |probe| {
        let Some(dispatch) = probe.next_request() else {
            return;
        };
        let stream = accept_before(&listener, dispatch.deadline());
        receive_upstream_request(&stream, dispatch.deadline());
        dispatch.complete(true);
        probe.observe(|deadline| matches!(deadline.io(stream).read(&mut [0]), Ok(0)));
    });
    let (root, server) = configured_server(&format!("http://{address}/v1"));
    let dispatched = observation.begin(
        Deadline::after(test_timeout(Duration::from_secs(3))),
        "upstream request ready",
    );
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        br#"{"model":"demo/model","input":[{"role":"user","content":"synthetic input"},{"type":"compaction_trigger"}],"stream":true}"#,
        &[&session_header(&server)],
    );
    assert!(dispatched.wait().unwrap());
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    let cancellation = observation.begin(deadline, "upstream cancellation");
    drop(downstream);
    let upstream_closed = cancellation.wait().unwrap();
    drop(observation);
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

#[track_caller]
pub(super) fn finished(root: &Path, count: usize) -> Vec<Value> {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(10)));
    let mut records = Vec::new();
    wait_for(&deadline, || {
        records = live_journal_until(root, &deadline);
        let actual = records
            .iter()
            .filter(|r| r["event"] == "request_finished")
            .count();
        assert!(
            actual <= count,
            "extra request receipts: expected {count}, found {actual}"
        );
        actual == count
    });
    records
}

fn chat_answer() -> Value {
    json!({"id":"chat_fixture","model":"upstream-model","choices":[{"index":0,
        "message":{"role":"assistant","content":"private-answer"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}})
}

fn request_phases<'a>(records: &'a [Value], request_id: &Value) -> Vec<&'a str> {
    records
        .iter()
        .filter(|record| {
            record["event"] == "request_phase_started"
                && record["fields"]["request_id"] == *request_id
        })
        .map(|record| record["fields"]["phase"].as_str().unwrap())
        .collect()
}

#[test]
fn http_and_compact_use_one_id_from_entry_through_dispatch_and_write() {
    for path in ["/v1/responses", "/v1/responses/compact"] {
        let upstream = OneShotUpstream::start(chat_answer());
        let (root, server) = configured_server(&upstream.base_url());
        let body = json!({"model":"demo/model","input":"private-question","reasoning":{"effort":"medium"}});
        let mut body = body;
        if path == "/v1/responses" {
            body["input"] = json!([
                {"type":"additional_tools","role":"developer","tools":[
                    {"type":"tool_search","execution":"client","parameters":{"type":"object"}},
                    {"type":"namespace","name":"files","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}
                ]},
                {"type":"tool_search_call","execution":"client","call_id":"s","arguments":{"query":"private-question"}},
                {"type":"tool_search_output","execution":"client","call_id":"s","status":"completed","tools":[]},
                {"type":"function_call","call_id":"f","namespace":"files","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"f","output":"private-question"}
            ]);
        }
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
            assert_eq!(sent["tools"].as_array().unwrap().len(), 2);
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
            "model_operation_started",
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
        let mut expected_phases = vec!["read_body", "resolve_route"];
        if path == "/v1/responses" {
            expected_phases.push("validate_input");
        }
        expected_phases.extend([
            "prepare_history",
            "prepare_destination",
            "execute_and_relay",
        ]);
        assert_eq!(
            request_phases(&records, &done["fields"]["request_id"]),
            expected_phases
        );
        let outcome = records
            .iter()
            .find(|r| r["event"] == "route_observation")
            .unwrap();
        if path == "/v1/responses" {
            assert_eq!(outcome["fields"]["tool_pairing_status"], "paired");
        }
        let report = request(
            &server,
            &format!(
                "/api/calls?start=0&end={}&request={id}",
                crate::util::system_now() + 1.0
            ),
            &[&session_header(&server)],
        );
        let report: Value = serde_json::from_str(report.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            report["records"].as_array().unwrap().len(),
            1,
            "one call receipt per operation"
        );
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
        vec![br#"{"error":{"type":"auth","message":"Subscription credential expired. token=private-upstream-failure","debug":"private-debug-value"}}"#.to_vec()],
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
    let rejection = records
        .iter()
        .find(|r| r["event"] == "upstream_request_rejected")
        .expect("recorded upstream cause");
    assert_eq!(
        rejection["fields"]["request_id"],
        done["fields"]["request_id"]
    );
    assert!(
        rejection["fields"]["upstream_message"]
            .as_str()
            .unwrap()
            .contains("Subscription credential expired")
    );
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-upstream-failure")
    );
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-debug-value")
    );
    assert!(!reply.contains("private-upstream-failure"));
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
    // Response writes can finish before their worker appends the receipt.
    for (status, phase) in [(401, "caller_access"), (400, "read_body")] {
        let receipt = done
            .iter()
            .find(|record| record["fields"]["response_status"] == status)
            .expect("receipt for the rejected response");
        assert_eq!(receipt["fields"]["last_phase"], phase);
    }
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
    let mut socket = emp_test_support::websocket::ClientWebSocket::connect(
        &format!("ws://{}/v1/responses", server.local_addr()),
        &headers,
        Duration::from_secs(3),
    )
    .unwrap();
    socket
        .send_json(&json!({"type":"private-invalid-type"}))
        .unwrap();
    assert_eq!(socket.receive_json().unwrap().unwrap()["type"], "error");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(300)));
    socket
        .send_json(&json!({"type":"response.create","input":"private-question"}))
        .unwrap();
    assert_eq!(
        websocket_event_before(&mut socket, &deadline),
        json!({
            "type":"error",
            "status":400,
            "error":{
                "code":"invalid_request",
                "origin":"emp",
                "message":"EMP 请求处理出错\n[EMP] request.model is required",
                "summary":"EMP 请求处理出错"
            }
        })
    );
    socket.send_json(&json!({"type":"response.create","model":"demo/model","input":"private-question","reasoning":{"effort":"low"}})).unwrap();
    loop {
        let event = websocket_event_before(&mut socket, &deadline);
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
    let records = finished(root.path(), 3);
    let done: Vec<_> = records
        .iter()
        .filter(|r| r["event"] == "request_finished")
        .collect();
    assert_ne!(
        done[0]["fields"]["request_id"],
        done[1]["fields"]["request_id"]
    );
    let connection_id = done[0]["fields"]["connection_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .expect("first turn has a nonempty connection ID");
    for turn in &done[1..] {
        assert_eq!(turn["fields"]["connection_id"], connection_id);
    }
    assert_eq!(done[0]["fields"]["downstream_terminal"], "error");
    assert_eq!(done[1]["fields"]["downstream_terminal"], "error");
    assert_eq!(
        request_phases(&records, &done[1]["fields"]["request_id"]),
        ["read_body", "resolve_route"]
    );
    assert_eq!(
        done[2]["fields"]["request_id"],
        sent_headers["x-emp-request-id"]
    );
    assert_eq!(done[2]["fields"]["downstream_terminal"], "completed");
    assert_eq!(
        request_phases(&records, &done[2]["fields"]["request_id"]),
        [
            "read_body",
            "resolve_route",
            "validate_input",
            "prepare_history",
            "prepare_destination",
            "execute_and_relay",
        ]
    );
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
    let root = emp_test_support::fixture_dir().unwrap();
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
    let (root, server) = configured_server("http://127.0.0.1:1/v1");
    let prepared = prepare_request(
        &server.state,
        json!({"model":"demo/model","input":"hello"}),
        RequestOperation::Responses,
        &Default::default(),
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
        let mut upstream = RequestGuard::new(
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

#[test]
fn ledger_keeps_execution_facts_after_activity_eviction() {
    use emp_router::native_http::{NativeRetryDecision, NativeRetryReason};
    let (_root, server) = configured_server("http://127.0.0.1:1/v1");
    let config = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()
        .clone();
    let route = emp_domain::resolve_route_without_catalog(&config, "demo/model").unwrap();
    let body = json!({"model":"demo/model","input":"private-fixture"});
    let headers = BTreeMap::from([("x-emp-request-id".into(), "long-request".into())]);
    let mut outcome = RequestGuard::new(&server.state, &route, &body, &headers, None, "responses");
    outcome.dispatch();
    outcome.native_retry(NativeRetryDecision {
        retry: true,
        status: 502,
        reason: NativeRetryReason::Network,
    });
    outcome.dispatch();
    // Fill the existing bounded projection while this request is still active.
    for index in 0..128 {
        let headers = BTreeMap::from([("x-emp-request-id".into(), format!("short-{index}"))]);
        let mut short =
            RequestGuard::new(&server.state, &route, &body, &headers, None, "responses");
        short.dispatch();
        short.http_status(200);
        short.finish();
    }
    let snapshot = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    assert!(
        !snapshot["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["request_id"] == "long-request")
    );
    outcome.http_status(200);
    outcome.finish();
    let now = crate::util::system_now();
    let report = server
        .state
        .backend
        .usage
        .ledger
        .query_calls(&emp_state::usage::ledger::CallFilter {
            start: now - 3600.0,
            end: now + 60.0,
            category: None,
            provider: None,
            account: None,
            model: None,
            models: vec![],
            session: None,
            state: None,
            request: Some("long-request".into()),
            offset: 0,
            limit: 10,
            models_offset: 0,
            models_sort: "calls".into(),
        })
        .unwrap();
    let receipt = &report["records"][0];
    assert_eq!(report["total"], 1);
    assert_eq!(receipt["state"], "completed");
    assert_eq!(receipt["attempts"], 2);
    assert_eq!(receipt["retries"][0]["reason"], "network");
    assert_eq!(receipt["attempt_routes"].as_array().unwrap().len(), 2);
    assert!(!report.to_string().contains("private-fixture"));
    drop(outcome);
    server.shutdown().unwrap();
}
