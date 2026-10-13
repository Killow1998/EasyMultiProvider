//! Large-context continuation through both real credential-selection paths.
use super::super::*;
use emp_transport::{ClientWebSocket, WebSocketPoll};
use sha2::{Digest, Sha256};
use std::sync::Mutex;

#[derive(Default, Debug)]
pub(super) struct Traffic {
    pub(super) ws_connections: usize,
    pub(super) http_requests: usize,
    pub(super) warmups: usize,
    pub(super) rejected_large: usize,
    pub(super) application_bytes: usize,
    pub(super) generated_hashes: Vec<String>,
    pub(super) cancelled_prefills: usize,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Fault {
    None,
    DisconnectIncremental,
    RejectIncremental,
    RejectPrefill,
    GeneratePrefill,
    SilentPrefill,
}

pub(super) struct Upstream {
    pub(super) address: SocketAddr,
    pub(super) traffic: Arc<Mutex<Traffic>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub(super) fn append_input(hash: &mut Sha256, body: &Value) {
    for item in body["input"].as_array().unwrap() {
        let bytes = serde_json::to_vec(item).unwrap();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
}

impl Upstream {
    pub(super) fn start(limit: usize) -> Self {
        Self::with_fault(limit, Fault::None)
    }

    pub(super) fn with_fault(limit: usize, fault: Fault) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let traffic = Arc::new(Mutex::new(Traffic::default()));
        let observed = Arc::clone(&traffic);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let fault_used = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                let traffic = Arc::clone(&observed);
                let stop = Arc::clone(&stopping);
                let fault_used = Arc::clone(&fault_used);
                workers.push(thread::spawn(move || {
                    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
                    let raw = read_request_head(&mut stream).unwrap();
                    let request = parse_request(&raw.head).unwrap();
                    assert_eq!(request.header("Authorization"), Some("Bearer fixture-access"));
                    assert_eq!(request.header("chatgpt-account-id"), Some("fixture-owner"));
                    if request.header("Upgrade").is_none() {
                        let size: usize = request.header("Content-Length").unwrap().parse().unwrap();
                        let encoding = request.header("Content-Encoding").unwrap_or("").to_owned();
                        let mut bytes = raw.body_prefix;
                        while bytes.len() < size {
                            let mut buffer = [0; 65536];
                            let count = stream.read(&mut buffer).unwrap();
                            assert_ne!(count, 0);
                            bytes.extend_from_slice(&buffer[..count]);
                        }
                        let bytes = decode_content(bytes, &encoding, 64 * 1024 * 1024, None).unwrap();
                        let body: Value = serde_json::from_slice(&bytes).unwrap();
                        let mut hash = Sha256::new();
                        append_input(&mut hash, &body);
                        let mut stats = traffic.lock().unwrap();
                        stats.http_requests += 1;
                        stats.application_bytes += bytes.len();
                        stats.generated_hashes.push(format!("{:x}", hash.finalize()));
                        let result = json!({"id":"resp_http","object":"response","status":"completed","model":"gpt-fixture","output":[]});
                        let result = serde_json::to_vec(&result).unwrap();
                        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",result.len()).unwrap();
                        stream.write_all(&result).unwrap();
                        return;
                    }
                    let accept = websocket_accept(request.header("Sec-WebSocket-Key").unwrap()).unwrap();
                    write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
                    stream.flush().unwrap();
                    let connection = {
                        let mut stats = traffic.lock().unwrap();
                        stats.ws_connections += 1;
                        stats.ws_connections
                    };
                    let mut ws = WebSocketConnection::new(&mut stream);
                    let mut hash = Sha256::new();
                    let mut last_id = String::new();
                    let mut sequence = 0;
                    while !stop.load(Ordering::Acquire) {
                        let text = match ws.poll_text_ready() {
                            Ok(WebSocketPoll::Text(text)) => text,
                            Ok(WebSocketPoll::Pending) => {
                                thread::sleep(Duration::from_millis(1));
                                continue;
                            }
                            Ok(WebSocketPoll::Ping(payload)) => {
                                ws.send_pong(&payload).unwrap();
                                continue;
                            }
                            _ => break,
                        };
                        traffic.lock().unwrap().application_bytes += text.len();
                        if text.len() > limit {
                            traffic.lock().unwrap().rejected_large += 1;
                            ws.close(1009, "fixture message limit");
                            break;
                        }
                        let body: Value = serde_json::from_str(&text).unwrap();
                        let warmup = body["generate"] == false;
                        let fault_applies = match fault {
                            Fault::DisconnectIncremental | Fault::RejectIncremental => body["previous_response_id"].is_string(),
                            Fault::RejectPrefill | Fault::GeneratePrefill | Fault::SilentPrefill => warmup,
                            Fault::None => false,
                        };
                        if fault_applies && !fault_used.swap(true, Ordering::AcqRel) {
                            match fault {
                                Fault::DisconnectIncremental => { ws.close(1001, "synthetic disconnect"); break; }
                                Fault::RejectIncremental => {
                                    ws.send_json(&json!({"type":"error","status":400,"error":{"code":"previous_response_not_found","message":"synthetic state loss"}})).unwrap();
                                    last_id.clear();
                                    continue;
                                }
                                Fault::RejectPrefill => {
                                    ws.send_json(&json!({"type":"error","status":429,"error":{"code":"rate_limit_exceeded","message":"synthetic prefill rejection"}})).unwrap();
                                    continue;
                                }
                                Fault::GeneratePrefill => {
                                    ws.send_json(&json!({"type":"response.output_text.delta","delta":"unexpected generation"})).unwrap();
                                    continue;
                                }
                                Fault::SilentPrefill => {
                                    traffic.lock().unwrap().warmups += 1;
                                    while !stop.load(Ordering::Acquire) {
                                        match ws.poll_text_ready() {
                                            Ok(WebSocketPoll::Pending) => thread::sleep(Duration::from_millis(1)),
                                            Ok(WebSocketPoll::Closed{..}) | Err(_) => {
                                                traffic.lock().unwrap().cancelled_prefills += 1;
                                                break;
                                            }
                                            _ => panic!("no next request while a prefill is pending"),
                                        }
                                    }
                                    break;
                                }
                                Fault::None => unreachable!(),
                            }
                        }
                        if let Some(previous) = body["previous_response_id"].as_str() {
                            assert_eq!(previous, last_id, "connection-local parent must match");
                        } else {
                            hash = Sha256::new();
                        }
                        append_input(&mut hash, &body);
                        {
                            let mut stats = traffic.lock().unwrap();
                            if warmup {
                                stats.warmups += 1;
                            } else {
                                stats.generated_hashes.push(format!("{:x}", hash.clone().finalize()));
                            }
                        }
                        sequence += 1;
                        last_id = format!("resp_{connection}_{sequence}");
                        ws.send_json(&json!({"type":"response.created","response":{"id":last_id,"status":"in_progress"}})).unwrap();
                        ws.send_json(&json!({"type":"response.completed","response":{"id":last_id,"object":"response","status":"completed","model":"gpt-fixture","output":[],"usage":{"input_tokens":41,"output_tokens":if warmup {0}else{1}}}})).unwrap();
                    }
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            address,
            traffic,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

pub(super) fn server(upstream: &Upstream, imported: bool) -> (TempDir, ServerHandle, String) {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native = root.join("codex/auth.json");
    std::fs::create_dir_all(native.parent().unwrap()).unwrap();
    let auth = json!({"tokens":{"access_token":"fixture-access","account_id":"fixture-owner"}});
    std::fs::write(&native, serde_json::to_vec(&auth).unwrap()).unwrap();
    let catalog = root.join("native-catalog.json");
    std::fs::write(&catalog, serde_json::to_vec(&json!({"models":[{"slug":"gpt-fixture","display_name":"Fixture","visibility":"list"}]})).unwrap()).unwrap();
    let mut config = json!({"codex_base_url":format!("http://{}/v1",upstream.address),"providers":[],"models":[],"accounts":[],"native_catalog_path":catalog});
    let auth_path = if imported {
        config["accounts"] =
            json!([{"id":"fixture","name":"Fixture","prefix":"fixture","enabled":true}]);
        let path = emp_state::account_auth_path(&config, "fixture", &config_path).unwrap();
        config["accounts"][0]["auth_file"] = json!(path);
        Some(path)
    } else {
        None
    };
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        native,
    )
    .unwrap();
    if let Some(path) = auth_path {
        server
            .state
            .backend
            .configuration
            .vault
            .write_encrypted_json(&path, &auth)
            .unwrap();
    }
    (
        directory,
        server,
        if imported {
            "fixture/gpt-fixture"
        } else {
            "gpt-fixture"
        }
        .into(),
    )
}

pub(super) fn connect(server: &ServerHandle, thread_id: &str) -> ClientWebSocket {
    let ws = ClientWebSocket::connect(
        &format!("ws://{}/v1/responses", server.local_addr()),
        &BTreeMap::from([
            ("X-EMP-Session".into(), server.session_token()),
            ("thread-id".into(), thread_id.into()),
            (
                "session-id".into(),
                "22222222-2222-4222-8222-222222222222".into(),
            ),
            (
                "OpenAI-Beta".into(),
                "responses_websockets=2026-02-06".into(),
            ),
        ]),
        Duration::from_secs(5),
    )
    .unwrap();
    ws.readiness_stream()
        .unwrap()
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    ws
}

pub(super) fn turn(client: &mut ClientWebSocket, body: &Value) -> Value {
    client.send_json(body).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(Instant::now() < deadline, "fixture turn did not terminate");
        let event: Value = match client.poll_receive_text().unwrap() {
            WebSocketPoll::Text(text) => serde_json::from_str(&text).unwrap(),
            WebSocketPoll::Pending => continue,
            other => panic!("unexpected fixture frame: {other:?}"),
        };
        if matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.failed" | "response.incomplete" | "error")
        ) {
            return event;
        }
    }
}

pub(super) fn initial_input() -> Vec<Value> {
    // Valid tiny PNGs plus text: deterministic 14 MiB context, no private data.
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aO1sAAAAASUVORK5CYII=";
    let mut input = Vec::new();
    for index in 0..28 {
        input.push(json!({"role":"user","content":[{"type":"input_text","text":format!("synthetic-{index}:{}","x".repeat(512*1024))},{"type":"input_image","image_url":image}]}));
        if index == 14 {
            input.push(json!({"type":"function_call","call_id":"fixture-call","name":"read_fixture","arguments":"{}"}));
            input.push(json!({"type":"function_call_output","call_id":"fixture-call","output":"synthetic result"}));
        }
    }
    input
}
