use super::support::*;
use super::*;

struct NativeWebSocketUpstream {
    address: SocketAddr,
    requests: mpsc::Receiver<(BTreeMap<String, String>, Value)>,
    worker: Option<JoinHandle<()>>,
}
impl NativeWebSocketUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = mpsc::sync_channel(2);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let raw = read_request_head(&mut stream).unwrap();
            let request = parse_request(&raw.head).unwrap();
            assert_eq!(request.target, "/v1/responses");
            let headers = request
                .headers
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect::<BTreeMap<_, _>>();
            let accept = websocket_accept(&headers["sec-websocket-key"]).unwrap();
            write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nOpenAI-Model: upstream\r\nX-Codex-Turn-State: native-ws-turn\r\nX-Models-Etag: stale-native-ws\r\n\r\n").unwrap();
            stream.flush().unwrap();
            let mut websocket = WebSocketConnection::new(&mut stream);
            for (index, id) in ["resp_one", "resp_two"].into_iter().enumerate() {
                let received = websocket.receive_text();
                let Ok(Some(text)) = received else {
                    return;
                };
                let body: Value = serde_json::from_str(&text).unwrap();
                sender.send((headers.clone(), body.clone())).unwrap();
                if index == 1 {
                    assert_eq!(body["previous_response_id"], "resp_one");
                }
                websocket.send_json(&json!({"type":"response.created","response":{"id":id,"status":"in_progress"}})).unwrap();
                if index == 0 {
                    websocket.send_json(&json!({"type":"response.output_text.delta","delta":"native websocket"})).unwrap();
                }
                websocket.send_json(&json!({"type":"response.completed","response":{"id":id,"object":"response","status":"completed","model":"upstream","output":[]} })).unwrap();
            }
        });
        Self {
            address,
            requests,
            worker: Some(worker),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}
impl Drop for NativeWebSocketUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
fn responses_websocket_reuses_matching_native_upstream_for_incremental_turn() {
    let upstream = NativeWebSocketUpstream::start();
    let (_directory, server) = native_alias_server(&upstream.base_url());
    let cookie = session_header(&server);
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: native-ws-thread\r\nx-openai-subagent: websocket-subagent\r\n\r\n",
        server.local_addr().port()
    )
    .unwrap();
    stream.flush().unwrap();
    let mut head = Vec::new();
    while !head.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 101"));
    send_masked_websocket_text(
        &mut stream,
        &json!({"type":"response.create","model":"native/alias","input":"first"}),
    );
    let mut first = Vec::new();
    loop {
        let event = receive_websocket_json(&mut stream);
        let done = event["type"] == "response.completed";
        first.push(event);
        if done {
            break;
        }
    }
    assert_eq!(first[0]["type"], "codex.response.metadata");
    assert!(
        first
            .iter()
            .any(|event| event["type"] == "response.metadata"
                && event["headers"]["openai-model"] == "native/alias")
    );
    assert!(
        first
            .iter()
            .any(|event| event["type"] == "response.output_text.delta")
    );
    send_masked_websocket_text(
        &mut stream,
        &json!({"type":"response.create","model":"native/alias","previous_response_id":"resp_one","input":[{"type":"message","role":"user","content":[]}]}),
    );
    let mut second = Vec::new();
    loop {
        let event = receive_websocket_json(&mut stream);
        let done = event["type"] == "response.completed";
        second.push(event);
        if done {
            break;
        }
    }
    assert_eq!(second[0]["type"], "codex.response.metadata");
    assert!(
        !second
            .iter()
            .any(|event| event["error"]["code"] == "previous_response_not_found")
    );
    assert_eq!(second.last().unwrap()["response"]["id"], "resp_two");
    let (first_headers, first_body) = upstream
        .requests
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (_, second_body) = upstream
        .requests
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert_eq!(first_headers["authorization"], "Bearer caller");
    assert_eq!(first_headers["thread-id"], "native-ws-thread");
    assert_eq!(first_headers["x-openai-subagent"], "websocket-subagent");
    assert_eq!(first_body["type"], "response.create");
    assert_eq!(first_body["model"], "upstream");
    assert_eq!(second_body["previous_response_id"], "resp_one");
    drop(stream);
    server.shutdown().unwrap();
}

struct NativeFailureContinuityUpstream {
    address: SocketAddr,
    requests: mpsc::Receiver<Value>,
    worker: Option<JoinHandle<()>>,
}

impl NativeFailureContinuityUpstream {
    fn start(failure_event: &'static str) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = mpsc::sync_channel(3);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let raw = read_request_head(&mut stream).unwrap();
            let request = parse_request(&raw.head).unwrap();
            let headers = request
                .headers
                .lines()
                .skip(1)
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
                .collect::<BTreeMap<_, _>>();
            let accept = websocket_accept(&headers["sec-websocket-key"]).unwrap();
            write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
            stream.flush().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut websocket = WebSocketConnection::new(&mut stream);
            for index in 0..3 {
                let text = match websocket.receive_text() {
                    Ok(Some(text)) => text,
                    Ok(None) | Err(_) => return,
                };
                let body: Value = serde_json::from_str(&text).unwrap();
                sender.send(body.clone()).unwrap();
                let id = if index == 0 {
                    "resp_one"
                } else if index == 1 {
                    "resp_failed"
                } else {
                    "resp_three"
                };
                websocket
                    .send_json(&json!({"type":"response.created","response":{"id":id,"status":"in_progress"}}))
                    .unwrap();
                if index == 1 {
                    let failure = if failure_event == "response.incomplete" {
                        json!({
                            "type":"response.incomplete",
                            "response":{"id":id,"object":"response","status":"incomplete",
                                "incomplete_details":{"reason":"max_output_tokens"},"output":[]}
                        })
                    } else {
                        json!({
                            "type":"response.failed",
                            "response":{"id":id,"object":"response","status":"failed",
                                "error":{"code":"rate_limit_exceeded","message":"retry later"},"output":[]}
                        })
                    };
                    websocket.send_json(&failure).unwrap();
                } else {
                    websocket
                        .send_json(&json!({"type":"response.completed","response":{"id":id,"object":"response","status":"completed","model":"upstream","output":[]}}))
                        .unwrap();
                }
            }
        });
        Self {
            address,
            requests,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}

impl Drop for NativeFailureContinuityUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn send_native_continuity_turn(
    stream: &mut TcpStream,
    input: &str,
    previous: Option<&str>,
) -> Vec<Value> {
    let mut body = json!({"type":"response.create","model":"native/alias","input":input});
    if let Some(previous) = previous {
        body["previous_response_id"] = Value::String(previous.to_owned());
    }
    send_masked_websocket_text(stream, &body);
    let mut events = Vec::new();
    loop {
        let event = receive_websocket_json(stream);
        let terminal = matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.failed" | "response.incomplete" | "error")
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

#[test]
fn responses_websocket_retains_last_successful_previous_id_after_failed_or_incomplete_turn() {
    for terminal in ["response.failed", "response.incomplete"] {
        let upstream = NativeFailureContinuityUpstream::start(terminal);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: continuity-thread\r\n\r\n",
            server.local_addr().port()
        )
        .unwrap();
        stream.flush().unwrap();
        let mut head = Vec::new();
        while !head.windows(4).any(|part| part == b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        assert!(String::from_utf8(head).unwrap().starts_with("HTTP/1.1 101"));

        let first = send_native_continuity_turn(&mut stream, "first", None);
        assert_eq!(first.last().unwrap()["type"], "response.completed");
        let failed = send_native_continuity_turn(&mut stream, "failed attempt", None);
        assert_eq!(failed.last().unwrap()["type"], terminal);
        let retry =
            send_native_continuity_turn(&mut stream, "continue previous success", Some("resp_one"));
        assert_eq!(
            retry.last().unwrap()["type"],
            "response.completed",
            "terminal {terminal} should leave resp_one as the last successful incremental base: {retry:?}"
        );
        assert!(
            !retry
                .iter()
                .any(|event| event["error"]["code"] == "previous_response_not_found"),
            "terminal {terminal} incorrectly discarded resp_one"
        );

        let observed = (0..3)
            .map(|_| {
                upstream
                    .requests
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(observed[2]["previous_response_id"], "resp_one");
        drop(stream);
        server.shutdown().unwrap();
    }
}
