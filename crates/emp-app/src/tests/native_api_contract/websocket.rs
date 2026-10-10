use super::support::*;
use super::*;

#[test]
fn responses_websocket_keeps_connection_and_requests_full_recovery_for_missing_previous() {
    let upstream = NativeSseUpstream::start(false);
    let (_directory, server) = native_alias_server(&upstream.base_url());
    let cookie = session_header(&server);
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = format!("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: websocket-thread\r\n\r\n",server.local_addr().port()).into_bytes();
    request.extend_from_slice(&masked_websocket_text(
        &json!({"type":"response.create","model":"native/alias","input":"hello"}),
    ));
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
    let mut handshake = Vec::new();
    while !handshake.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).unwrap();
        handshake.push(byte[0]);
    }
    let handshake = String::from_utf8(handshake).unwrap();
    assert!(handshake.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
    assert!(handshake.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
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
fn responses_websocket_disconnect_cancels_external_http_stream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_ready) = mpsc::sync_channel::<()>(1);
    let (closed_sender, closed) = mpsc::sync_channel::<bool>(1);
    let worker = thread::spawn(move || {
        let (mut upstream, _) = listener.accept().unwrap();
        let raw = read_request_head(&mut upstream).unwrap();
        let request = parse_request(&raw.head).unwrap();
        assert_eq!(request.raw_path(), "/v1/responses");
        let length = request
            .header("Content-Length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = raw.body_prefix;
        while body.len() < length {
            let mut buffer = [0_u8; 4096];
            let count = upstream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "external request body ended early");
            body.extend_from_slice(&buffer[..count]);
        }
        upstream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        upstream.flush().unwrap();
        upstream
            .write_all(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_ws\",\"status\":\"in_progress\",\"model\":\"upstream\"}}\n\n")
            .unwrap();
        upstream.flush().unwrap();
        request_sender.send(()).unwrap();
        upstream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut byte = [0_u8; 1];
        let ended = match upstream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        closed_sender.send(ended).unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
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
    let server =
        ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config).unwrap();
    let cookie = session_header(&server);
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = format!("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: external-ws-thread\r\n\r\n",server.local_addr().port()).into_bytes();
    request.extend_from_slice(&masked_websocket_text(
        &json!({"type":"response.create","model":"demo/model","input":"hello"}),
    ));
    stream.write_all(&request).unwrap();
    stream.flush().unwrap();
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
    request_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(stream);
    assert!(
        closed.recv_timeout(Duration::from_secs(5)).unwrap(),
        "EMP retained the external HTTP stream after the websocket client disconnected"
    );
    server.shutdown().unwrap();
    worker.join().unwrap();
}
