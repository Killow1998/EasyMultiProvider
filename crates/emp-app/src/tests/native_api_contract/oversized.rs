use super::support::*;
use super::*;

struct NativeTooLargeFallbackUpstream {
    address: SocketAddr,
    requests: mpsc::Receiver<(String, BTreeMap<String, String>, Value)>,
    worker: Option<JoinHandle<()>>,
}

impl NativeTooLargeFallbackUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (sender, requests) = mpsc::sync_channel(2);
        let worker = thread::spawn(move || {
            let Some(mut stream) = accept_fixture_connection(&listener) else {
                return;
            };
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
            let mut websocket = WebSocketConnection::new(&mut stream);
            let text = websocket
                .receive_text()
                .unwrap()
                .expect("native request frame");
            let body: Value = serde_json::from_str(&text).unwrap();
            sender
                .send((request.target.to_owned(), headers, body))
                .unwrap();
            websocket.close(1009, "fixture peer message limit");
            drop(websocket);
            drop(stream);

            let Some(mut fallback) = accept_fixture_connection(&listener) else {
                return;
            };
            let observed = receive_native_request(&mut fallback);
            sender
                .send((observed.path.clone(), observed.headers, observed.body))
                .unwrap();
            let response = br#"{"id":"resp_http_fallback","object":"response","status":"completed","model":"upstream","output":[]}"#;
            write!(fallback,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",response.len()).unwrap();
            fallback.write_all(response).unwrap();
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

fn accept_fixture_connection(listener: &TcpListener) -> Option<TcpStream> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(true)
                    .expect("nonblocking accepted native stream");
                stream
                    .set_nonblocking(false)
                    .expect("blocking accepted native stream");
                return Some(stream);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

impl Drop for NativeTooLargeFallbackUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn native_websocket_peer_1009_before_events_falls_back_to_full_http_request() {
    let upstream = NativeTooLargeFallbackUpstream::start();
    let (_directory, server) = native_alias_server(&upstream.base_url());
    let cookie = session_header(&server);
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: size-fallback-thread\r\n\r\n",
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
        &json!({"type":"response.create","model":"native/alias","input":"full request fallback"}),
    );
    let mut events = Vec::new();
    loop {
        let event = receive_websocket_json(&mut stream);
        let terminal = matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.failed" | "error")
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    assert_eq!(
        events.last().unwrap()["type"],
        "response.completed",
        "a pre-output peer 1009 must use the safe HTTP fallback: {events:?}"
    );
    let observations = (0..2)
        .map(|_| {
            upstream
                .requests
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(observations[0].0, "/v1/responses");
    assert_eq!(observations[0].2["type"], "response.create");
    assert_eq!(observations[0].2["model"], "upstream");
    assert_eq!(observations[1].0, "/v1/responses");
    assert_eq!(observations[1].1["content-encoding"], "zstd");
    assert_eq!(observations[1].2["model"], "upstream");
    assert_eq!(observations[1].2["stream"], true);
    drop(stream);
    server.shutdown().unwrap();
}
