use super::*;

pub(super) struct ImageUpstream {
    pub(super) address: SocketAddr,
    pub(super) observed: mpsc::Receiver<ObservedNativeRequest>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ImageUpstream {
    pub(super) fn start(status: u16) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, observed) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(socket) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let raw = read_request_head(&mut socket).unwrap();
                let head = parse_request(&raw.head).unwrap();
                if head
                    .header("upgrade")
                    .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
                {
                    let headers = head
                        .headers
                        .lines()
                        .skip(1)
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| {
                            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect::<BTreeMap<_, _>>();
                    let accept = websocket_accept(&headers["sec-websocket-key"]).unwrap();
                    write!(socket,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
                    socket.flush().unwrap();
                    let mut websocket = WebSocketConnection::new(&mut socket);
                    let body: Value =
                        serde_json::from_str(&websocket.receive_text().unwrap().unwrap()).unwrap();
                    sender
                        .send(ObservedNativeRequest {
                            path: head.target.to_owned(),
                            headers,
                            body,
                        })
                        .unwrap();
                    websocket.send_json(&json!({"type":"response.created","response":{"id":"response_fixture","status":"in_progress"}})).unwrap();
                    websocket.send_json(&json!({"type":"response.completed","response":{"id":"response_fixture","object":"response","status":"completed","model":"upstream","output":[]}})).unwrap();
                    continue;
                }
                let request = finish_native_request(&mut socket, raw);
                let image = request.path.contains("/images/");
                let streaming = request.body["stream"] == true;
                sender.send(request).unwrap();
                let status = if image { status } else { 200 };
                let body = if !image {
                    let value = json!({"id":"response_fixture", "object":"response", "status":"completed", "model":"upstream", "output":[]});
                    if streaming {
                        format!(
                            "data: {}\n\n",
                            json!({"type":"response.completed", "response":value})
                        )
                    } else {
                        value.to_string()
                    }
                } else if status == 200 {
                    json!({"created":1,"data":[{"b64_json":"ZmFrZQ==", "generation_id":"generation_fixture"}],
                        "usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6},"future":{"opaque":true}}).to_string()
                } else {
                    json!({"error":{"type":"usage_limit_reached", "message":"synthetic image limit", "resets_at":2100000000}}).to_string()
                };
                let content_type = if streaming && !image {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                write!(socket,"HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nx-codex-imagegen-request-id: image-request-fixture\r\nx-codex-active-limit: image_gen\r\nx-image-gen-primary-used-percent: 100\r\nRetry-After: 17\r\nSet-Cookie: must-not-forward\r\nConnection: close\r\n\r\n{body}",status_text(status),body.len()).unwrap();
            }
        });
        Self {
            address,
            observed,
            stop,
            worker: Some(worker),
        }
    }

    pub(super) fn base_url(&self) -> String {
        format!("http://{}/backend-api/codex", self.address)
    }
    pub(super) fn next(&self) -> ObservedNativeRequest {
        self.observed.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}

impl Drop for ImageUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

pub(super) fn image_server(base_url: &str) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().unwrap();
    let root = crate::tests::credential_root(&directory);
    let config_path = root.join("config.json");
    let home = root.join("codex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("auth.json"),
        serde_json::to_vec(&json!({"tokens":{
        "access_token":"native-fixture", "account_id":"native-owner"}}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join("models.json"),
        serde_json::to_vec(&json!({"models":[{"slug":"gpt-6.1-sol"}]})).unwrap(),
    )
    .unwrap();
    let mut config = json!({"codex_base_url":base_url, "native_catalog_path":home.join("models.json"),
        "providers":[],"models":[],"accounts":[
        {"id":"selected","name":"Selected","prefix":"selected","enabled":true},
        {"id":"other","name":"Other","prefix":"other","enabled":true}]});
    for index in 0..config["accounts"].as_array().unwrap().len() {
        let id = config["accounts"][index]["id"].as_str().unwrap();
        let path = emp_state::account_auth_path(&config, id, &config_path).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        config["accounts"][index]["auth_file"] = json!(path);
    }
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        home.join("auth.json"),
    )
    .unwrap();
    for account in config["accounts"].as_array().unwrap() {
        let id = account["id"].as_str().unwrap();
        server.state.backend.configuration.vault.write_encrypted_json(Path::new(account["auth_file"].as_str().unwrap()),
            &json!({"tokens":{"access_token":format!("{id}-fixture"),"account_id":format!("{id}-owner")}})).unwrap();
    }
    (directory, server)
}

pub(super) fn select_turn(server: &ServerHandle, model: &str, turn: &str, websocket: bool) {
    let body = json!({"model":model,"input":[], "client_metadata":{
        "x-codex-turn-metadata":json!({"turn_id":turn}).to_string()}});
    if !websocket {
        let response = post(
            server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &[&session_header(server)],
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        return;
    }
    let mut socket = TcpStream::connect(server.local_addr()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(socket,"GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{}\r\n\r\n",server.local_addr().port(),session_header(server)).unwrap();
    let mut handshake = Vec::new();
    while !handshake.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        handshake.push(byte[0]);
    }
    assert!(handshake.starts_with(b"HTTP/1.1 101"));
    let mut body = body;
    body["type"] = json!("response.create");
    send_masked_websocket_text(&mut socket, &body);
    loop {
        let event = receive_websocket_json(&mut socket);
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "response.completed" {
            break;
        }
    }
}
