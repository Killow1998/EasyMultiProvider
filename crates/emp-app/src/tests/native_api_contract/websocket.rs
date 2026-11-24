use super::support::*;
use super::*;
use emp_test_support::test_timeout;

#[test]
fn responses_websocket_keeps_connection_and_requests_full_recovery_for_missing_previous() {
    let upstream = NativeSseUpstream::start(false);
    let (_directory, server) = native_alias_server(&upstream.base_url());
    let cookie = session_header(&server);
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    let mut request = format!("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\noriginator: codex_cli_rs\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: websocket-thread\r\n\r\n",server.local_addr().port()).into_bytes();
    request.extend_from_slice(&masked_websocket_text(
        &json!({"type":"response.create","model":"native/alias","input":"hello"}),
    ));
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut stream = Deadline::after(test_timeout(Duration::from_secs(5))).io(stream);
    let mut handshake = Vec::new();
    while !handshake.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).unwrap();
        handshake.push(byte[0]);
    }
    let handshake = String::from_utf8(handshake).unwrap();
    assert!(handshake.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
    assert_eq!(
        response_header(&handshake, "sec-websocket-accept"),
        Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        "{handshake}"
    );
    let mut events = Vec::new();
    loop {
        let event = receive_websocket_json(&mut stream);
        let terminal = event["type"] == "response.completed";
        events.push(event);
        if terminal {
            break;
        }
    }
    assert_eq!(events[0]["type"], "codex.response.metadata");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.metadata")
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.output_text.delta")
    );
    assert_eq!(events.last().unwrap()["response"]["model"], "upstream");
    send_masked_websocket_text(
        &mut stream,
        &json!({"type":"response.create","model":"native/alias","previous_response_id":"resp_stream","input":[]}),
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "codex.response.metadata"
    );
    let recovery = receive_websocket_json(&mut stream);
    assert_eq!(recovery["error"]["code"], "previous_response_not_found");
    send_masked_websocket_text(
        &mut stream,
        &json!({"type":"response.create","model":"native/alias","generate":false,"input":[]}),
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "codex.response.metadata"
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "response.created"
    );
    let warmup = receive_websocket_json(&mut stream);
    assert_eq!(warmup["type"], "response.completed");
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
            state: Some("recovery_required".into()),
            request: None,
            offset: 0,
            limit: 50,
            models_offset: 0,
            models_sort: "calls".into(),
        })
        .unwrap();
    assert_eq!(report["total"], 1, "{report}");
    assert_eq!(
        report["records"][0]["error_code"],
        "previous_response_not_found"
    );
    assert_eq!(report["records"][0]["error_origin"], "emp");
    let activity = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    assert!(
        activity["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["state"] == "recovery_required"
                && r["error_code"] == "previous_response_not_found")
    );
    assert_eq!(
        warmup["response"]["id"], "",
        "HTTP warmup has no resumable server history"
    );
    let mask = [5u8, 6, 7, 8];
    let close = [0x03u8, 0xe8];
    let mut frame = vec![0x88, 0x80 | 2];
    frame.extend_from_slice(&mask);
    frame.extend(
        close
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
    let observed = upstream.observed();
    assert_eq!(observed.headers["authorization"], "Bearer caller");
    assert_eq!(observed.headers["thread-id"], "websocket-thread");
    assert_eq!(observed.body["stream"], true);
    drop(stream);
    server.shutdown().unwrap();
}

#[test]
fn responses_websocket_streams_first_external_event_and_cancels_on_disconnect() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let observation = Observation::spawn(move |probe| {
        let Some(dispatch) = probe.next_request() else {
            return;
        };
        let mut upstream = accept_before(&listener, dispatch.deadline());
        let raw = read_request_head(&mut upstream).unwrap();
        let request = parse_request(&raw.head).unwrap();
        assert_eq!(request.raw_path(), "/v1/responses");
        let length = request
            .header("Content-Length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = raw.body_prefix;
        let mut io = dispatch.deadline().io(&upstream);
        while body.len() < length {
            let mut buffer = [0_u8; 4096];
            let count = io.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "external request body ended early");
            body.extend_from_slice(&buffer[..count]);
        }
        io.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
        io.flush().unwrap();
        io
            .write_all(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_ws\",\"status\":\"in_progress\",\"model\":\"upstream\"}}\n\n")
            .unwrap();
        io.flush().unwrap();
        dispatch.complete(true);
        probe.observe(|deadline| upstream_closed_before(&upstream, deadline));
    });
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "providers":[{"id":"demo","name":"Demo","base_url":format!("http://{address}/v1"),"protocol":"responses","auth_mode":"api_key","api_key":"upstream-secret"}],
            "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream-model","enabled":true}]
        }))
        .unwrap(),
    )
    .unwrap();
    let server = ServerHandle::start_for_test(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        &config,
        root.join("codex/auth.json"),
    )
    .unwrap();
    let cookie = session_header(&server);
    let address = server.local_addr();
    let mut stream = TcpStream::connect(address).unwrap();
    let mut request = format!("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\noriginator: codex_cli_rs\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: external-ws-thread\r\n\r\n",server.local_addr().port()).into_bytes();
    request.extend_from_slice(&masked_websocket_text(
        &json!({"type":"response.create","model":"demo/model","input":"hello"}),
    ));
    let dispatched = observation.begin(
        Deadline::after(test_timeout(Duration::from_secs(2))),
        "upstream request ready",
    );
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut stream = Deadline::after(test_timeout(Duration::from_secs(5))).io(stream);
    let mut handshake = Vec::new();
    while !handshake.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).unwrap();
        handshake.push(byte[0]);
    }
    assert!(
        String::from_utf8(handshake)
            .unwrap()
            .starts_with("HTTP/1.1 101")
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "codex.response.metadata"
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "response.created"
    );
    assert!(dispatched.wait().unwrap());
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let cancellation = observation.begin(deadline, "upstream cancellation");
    drop(stream);
    assert!(
        cancellation.wait().unwrap(),
        "EMP retained the external HTTP stream after the websocket client disconnected"
    );
    server.shutdown().unwrap();
    drop(observation);
}

#[test]
fn generic_websocket_client_receives_only_public_responses_events() {
    let upstream = NativeSseUpstream::start(false);
    let (_directory, server) = native_alias_server(&upstream.base_url());
    let mut socket = TcpStream::connect(server.local_addr()).unwrap();
    write!(socket, "GET /v1/responses HTTP/1.1\r\nHost: {}\r\n{}\r\nAuthorization: Bearer fixture\r\nUser-Agent: async-openai\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",server.local_addr(),session_header(&server)).unwrap();
    let mut socket = Deadline::after(test_timeout(Duration::from_secs(5))).io(socket);
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 101"));
    send_masked_websocket_text(
        &mut socket,
        &json!({"type":"response.create","model":"native/alias","input":"hello"}),
    );
    loop {
        let event = receive_websocket_json(&mut socket);
        let kind = event["type"].as_str().unwrap();
        assert!(
            matches!(
                kind,
                "response.created" | "response.output_text.delta" | "response.completed"
            ),
            "{event}"
        );
        if kind == "response.completed" {
            break;
        }
    }
    drop(socket);
    server.shutdown().unwrap();
}
