//! Codex's create -> interrupt -> incomplete -> incremental continuation contract.
use super::support::*;
use super::*;
use emp_transport::ClientWebSocket;

fn accept_upstream(listener: TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let raw = read_request_head(&mut stream).unwrap();
    let request = parse_request(&raw.head).unwrap();
    let accept = websocket_accept(request.header("Sec-WebSocket-Key").unwrap()).unwrap();
    write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
    stream.flush().unwrap();
    stream
}

fn connect(server: &ServerHandle) -> ClientWebSocket {
    let client = ClientWebSocket::connect(
        &format!("ws://{}/v1/responses", server.local_addr()),
        &BTreeMap::from([
            ("X-EMP-Session".into(), server.session_token().to_owned()),
            ("Authorization".into(), "Bearer fixture".into()),
            ("thread-id".into(), "interrupt-fixture".into()),
        ]),
        Duration::from_secs(5),
    )
    .unwrap();
    client
        .readiness_stream()
        .unwrap()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    client
}

fn event(client: &mut ClientWebSocket, kind: &str) -> Value {
    // Bound even broken implementations whose receive_json retries socket timeouts.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "waiting for {kind}");
        match client.poll_receive_text().unwrap() {
            emp_transport::WebSocketPoll::Text(text) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return value;
                }
                assert_ne!(value["type"], "error", "{value}");
                assert_ne!(value["type"], "response.failed", "{value}");
            }
            emp_transport::WebSocketPoll::Pending => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

fn request(ws: &mut WebSocketConnection<'_, TcpStream>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            Instant::now() < deadline,
            "upstream did not receive a request"
        );
        match ws.poll_text().unwrap() {
            emp_transport::WebSocketPoll::Text(text) => {
                return serde_json::from_str(&text).unwrap();
            }
            emp_transport::WebSocketPoll::Pending => {}
            other => panic!("unexpected upstream frame: {other:?}"),
        }
    }
}

#[test]
fn local_and_upstream_websocket_errors_have_distinct_receipts_and_preserve_upstream_wire() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let upstream_error = json!({"type":"error","status":429,"error":{
        "code":"rate_limit_exceeded","origin":"emp","message":"private-upstream-detail"
    }});
    let expected = upstream_error.clone();
    let worker = thread::spawn(move || {
        let mut stream = accept_upstream(listener);
        let mut ws = WebSocketConnection::new(&mut stream);
        request(&mut ws);
        ws.send_json(&upstream_error).unwrap();
    });
    let (root, server) = native_alias_server(&base_url);
    let mut client = connect(&server);
    client
        .send_json(&json!({"type":"invalid-control"}))
        .unwrap();
    let local = event(&mut client, "error");
    assert_eq!(local["error"]["origin"], "emp");
    assert!(
        local["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("[EMP]")
    );
    client
        .send_json(&json!({"type":"response.create","model":"native/alias","input":"first"}))
        .unwrap();
    assert_eq!(event(&mut client, "error"), expected);
    let activity = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    let failed = activity["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["state"] == "failed")
        .unwrap();
    assert_eq!(
        failed["error_origin"], "upstream",
        "upstream cannot claim EMP origin"
    );
    assert_eq!(failed["http_status"], 429);
    assert_eq!(failed["error_code"], "rate_limit_exceeded");
    drop(client);
    worker.join().unwrap();
    let report = server
        .state
        .backend
        .usage
        .ledger
        .query_calls(&emp_state::usage::ledger::CallFilter {
            start: 0.0,
            end: crate::util::system_now(),
            category: None,
            provider: None,
            account: None,
            model: None,
            models: vec![],
            session: None,
            state: Some("failed".into()),
            request: None,
            offset: 0,
            limit: 50,
            models_offset: 0,
            models_sort: "calls".into(),
        })
        .unwrap();
    assert_eq!(report["summary"]["failed"], 1);
    assert_eq!(report["records"][0]["error_origin"], "upstream");
    server.shutdown().unwrap();
    let journal = crate::tests::internal_events_contract::journal(root.path());
    for (code, origin) in [
        ("invalid_request", "emp"),
        ("rate_limit_exceeded", "upstream"),
    ] {
        let error = journal
            .iter()
            .find(|event| {
                event["event"] == "response_error" && event["fields"]["error_code"] == code
            })
            .unwrap();
        assert_eq!(error["fields"]["error_origin"], origin);
        assert_eq!(error["fields"]["delivered"], true);
        assert!(error["fields"]["request_id"].is_string());
        assert!(error["fields"]["phase"].is_string());
    }
    assert!(
        !serde_json::to_string(&journal)
            .unwrap()
            .contains("private-upstream-detail")
    );
}

#[test]
fn interrupt_is_forwarded_during_generation_and_its_response_can_be_continued() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let interrupt = json!({"type":"response.interrupt","response_id":"resp_interrupted",
        "mode":"discard_partial_items","future_control":{"keep":true}});
    let expected = interrupt.clone();
    let worker = thread::spawn(move || {
        let mut stream = accept_upstream(listener);
        let mut ws = WebSocketConnection::new(&mut stream);
        let create = request(&mut ws);
        assert_eq!(create["model"], "upstream");
        ws.send_json(&json!({"type":"response.created","response":{"id":"resp_interrupted"}}))
            .unwrap();
        ws.send_json(
            &json!({"type":"response.output_item.added","item":{"type":"message",
            "id":"partial","role":"assistant","phase":"partial_answer","content":[]}}),
        )
        .unwrap();
        // A later read must be the interrupt, not a repeated inference request.
        let control = request(&mut ws);
        assert_eq!(control, expected);
        ws.send_json(
            &json!({"type":"response.incomplete","response":{"id":"resp_interrupted",
            "status":"incomplete","incomplete_details":{"reason":"interrupted"},"end_turn":false,
            "usage":{"input_tokens":7,"output_tokens":2},"future_response":{"keep":true}}}),
        )
        .unwrap();
        let next = request(&mut ws);
        assert_eq!(next["type"], "response.create");
        assert_eq!(next["previous_response_id"], "resp_interrupted");
        assert_eq!(next["model"], "upstream");
        ws.send_json(
            &json!({"type":"response.completed","response":{"id":"resp_next",
            "status":"completed","output":[],"end_turn":false}}),
        )
        .unwrap();
    });
    let (_root, server) = native_alias_server(&base_url);
    let mut client = connect(&server);
    client
        .send_json(&json!({"type":"response.create","model":"native/alias","input":"first"}))
        .unwrap();
    assert_eq!(
        event(&mut client, "response.created")["response"]["id"],
        "resp_interrupted"
    );
    assert_eq!(
        event(&mut client, "response.output_item.added")["item"]["phase"],
        "partial_answer"
    );
    client.send_json(&interrupt).unwrap();
    let incomplete = event(&mut client, "response.incomplete");
    assert_eq!(
        incomplete["response"]["incomplete_details"]["reason"],
        "interrupted"
    );
    assert_eq!(incomplete["response"]["usage"]["input_tokens"], 7);
    assert_eq!(incomplete["response"]["future_response"]["keep"], true);
    assert_eq!(incomplete["response"]["end_turn"], false);
    client
        .send_json(&json!({"type":"response.create","model":"native/alias",
        "previous_response_id":"resp_interrupted","input":"continue"}))
        .unwrap();
    let completed = event(&mut client, "response.completed");
    assert_eq!(completed["response"]["id"], "resp_next");
    assert_eq!(completed["response"]["end_turn"], false);
    drop(client);
    worker.join().unwrap();
    server.shutdown().unwrap();
}

#[test]
fn disconnect_cancels_a_native_websocket_while_its_upstream_is_silent() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (closed_tx, closed) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut stream = accept_upstream(listener);
        let mut ws = WebSocketConnection::new(&mut stream);
        request(&mut ws);
        ws.send_json(&json!({"type":"response.created","response":{"id":"resp_waiting"}}))
            .unwrap();
        closed_tx
            .send(matches!(
                ws.poll_text(),
                Ok(emp_transport::WebSocketPoll::Closed { .. })
            ))
            .unwrap();
    });
    let (_root, server) = native_alias_server(&base_url);
    let mut client = connect(&server);
    client
        .send_json(&json!({"type":"response.create","model":"native/alias","input":"wait"}))
        .unwrap();
    event(&mut client, "response.created");
    drop(client);
    assert!(
        closed.recv_timeout(Duration::from_secs(2)).unwrap(),
        "upstream was not cancelled"
    );
    worker.join().unwrap();
    server.shutdown().unwrap();
}

#[test]
fn next_create_queued_during_a_native_turn_is_dispatched_after_its_terminal() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (release, wait) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut stream = accept_upstream(listener);
        let mut ws = WebSocketConnection::new(&mut stream);
        request(&mut ws);
        ws.send_json(&json!({"type":"response.created","response":{"id":"resp_first"}}))
            .unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        ws.send_json(&json!({"type":"response.completed","response":{"id":"resp_first","status":"completed","output":[]}})).unwrap();
        let next = request(&mut ws);
        assert_eq!(next["input"], "second");
        ws.send_json(&json!({"type":"response.completed","response":{"id":"resp_second","status":"completed","output":[]}})).unwrap();
    });
    let (_root, server) = native_alias_server(&base_url);
    let mut client = connect(&server);
    client
        .send_json(&json!({"type":"response.create","model":"native/alias","input":"first"}))
        .unwrap();
    event(&mut client, "response.created");
    client
        .send_json(&json!({"type":"response.create","model":"native/alias","input":"second"}))
        .unwrap();
    release.send(()).unwrap();
    assert_eq!(
        event(&mut client, "response.completed")["response"]["id"],
        "resp_first"
    );
    assert_eq!(
        event(&mut client, "response.completed")["response"]["id"],
        "resp_second"
    );
    drop(client);
    worker.join().unwrap();
    server.shutdown().unwrap();
}

#[test]
fn incremental_disconnect_after_output_does_not_request_a_full_replay() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let mut stream = accept_upstream(listener);
        let mut ws = WebSocketConnection::new(&mut stream);
        request(&mut ws);
        ws.send_json(
            &json!({"type":"response.completed","response":{"id":"resp_base",
            "status":"completed","output":[]}}),
        )
        .unwrap();
        let next = request(&mut ws);
        assert_eq!(next["previous_response_id"], "resp_base");
        ws.send_json(&json!({"type":"response.created","response":{"id":"resp_partial"}}))
            .unwrap();
        ws.send_json(&json!({"type":"response.output_text.delta","delta":"partial"}))
            .unwrap();
        // Lose the upstream before a terminal. EMP must not pretend that the
        // previous response was missing after this request already produced output.
    });
    let (_root, server) = native_alias_server(&base_url);
    let mut client = connect(&server);
    client
        .send_json(&json!({"type":"response.create","model":"native/alias","input":"first"}))
        .unwrap();
    event(&mut client, "response.completed");
    client
        .send_json(&json!({"type":"response.create","model":"native/alias",
        "previous_response_id":"resp_base","input":"next"}))
        .unwrap();
    assert_eq!(
        event(&mut client, "response.output_text.delta")["delta"],
        "partial"
    );
    let failure = event(&mut client, "response.failed");
    assert_eq!(
        failure["response"]["error"]["failure_reason"],
        "stream_incomplete"
    );
    assert_ne!(
        failure["response"]["error"]["code"],
        "previous_response_not_found"
    );
    drop(client);
    worker.join().unwrap();
    server.shutdown().unwrap();
}
