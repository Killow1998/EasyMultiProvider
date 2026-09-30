//! Shared native request observation and endpoint fixtures.
use super::*;

#[derive(Debug)]
pub(super) struct ObservedNativeRequest {
    pub(super) path: String,
    pub(super) headers: BTreeMap<String, String>,
    pub(super) body: Value,
}

pub(super) fn receive_native_request(stream: &mut TcpStream) -> ObservedNativeRequest {
    let raw = read_request_head(stream).expect("native request head");
    finish_native_request(stream, raw)
}

pub(super) fn finish_native_request(
    stream: &mut TcpStream,
    raw: RequestHead,
) -> ObservedNativeRequest {
    let request = parse_request(&raw.head).expect("native HTTP request");
    let headers = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let length = headers["content-length"]
        .parse::<usize>()
        .expect("native Content-Length");
    let mut encoded = raw.body_prefix;
    while encoded.len() < length {
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).expect("native request body");
        assert!(count > 0);
        encoded.extend_from_slice(&chunk[..count]);
    }
    encoded.truncate(length);
    let decoded = decode_content(
        encoded,
        headers
            .get("content-encoding")
            .map(String::as_str)
            .unwrap_or(""),
        4 * 1024 * 1024,
        None,
    )
    .expect("decode native zstd");
    ObservedNativeRequest {
        path: request.target.to_owned(),
        headers,
        body: serde_json::from_slice(&decoded).expect("native request JSON"),
    }
}

pub(super) struct NativeUpstream {
    pub(super) address: SocketAddr,
    pub(super) observed: mpsc::Receiver<ObservedNativeRequest>,
    pub(super) worker: Option<JoinHandle<()>>,
    pub(super) raw_response: Vec<u8>,
}

impl NativeUpstream {
    pub(super) fn start(requests: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind native upstream");
        let address = listener.local_addr().expect("native upstream address");
        let (sender, observed) = mpsc::sync_channel(requests);
        let raw_response = br#"{ "id":"resp_native", "object":"response", "status":"completed", "model":"upstream", "output":[], "future":{"opaque":[1,true,"x"]} }"#.to_vec();
        let response = raw_response.clone();
        let worker = thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().expect("accept native upstream");
                let raw = read_request_head(&mut stream).expect("native request head");
                let request = parse_request(&raw.head).expect("native HTTP request");
                let headers = request
                    .headers
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| {
                        (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                    })
                    .collect::<BTreeMap<_, _>>();
                let length = headers
                    .get("content-length")
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("native Content-Length");
                let mut encoded = raw.body_prefix;
                while encoded.len() < length {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).expect("read native request");
                    assert!(count > 0, "native request ended before body");
                    encoded.extend_from_slice(&chunk[..count]);
                }
                encoded.truncate(length);
                let body = decode_content(
                    encoded,
                    headers
                        .get("content-encoding")
                        .map(String::as_str)
                        .unwrap_or(""),
                    4 * 1024 * 1024,
                    None,
                )
                .expect("decode zstd native request");
                sender
                    .send(ObservedNativeRequest {
                        path: request.target.to_owned(),
                        headers,
                        body: serde_json::from_slice(&body).expect("native request JSON"),
                    })
                    .expect("record native request");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nOpenAI-Model: upstream\r\nX-Codex-Turn-State: fixture-turn\r\nX-Models-Etag: stale-upstream-etag\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .expect("write native response head");
                stream.write_all(&response).expect("write native response");
            }
        });
        Self {
            address,
            observed,
            worker: Some(worker),
            raw_response,
        }
    }

    pub(super) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    pub(super) fn next(&self) -> ObservedNativeRequest {
        self.observed
            .recv_timeout(Duration::from_secs(5))
            .expect("observed native request")
    }
}

impl Drop for NativeUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join native upstream");
        }
    }
}

pub(super) fn forward_server(base_url: &str) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    let native = root.join("codex/auth.json");
    std::fs::create_dir_all(native.parent().unwrap()).unwrap();
    std::fs::write(&config,serde_json::to_vec(&json!({"providers":[{"id":"native","base_url":base_url,"protocol":"responses","auth_mode":"forward"}],"models":[]})).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-test-codex",
        native,
    )
    .unwrap();
    (directory, server)
}

pub(super) fn native_alias_server(base_url: &str) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    let native = root.join("codex/auth.json");
    std::fs::create_dir_all(native.parent().unwrap()).unwrap();
    std::fs::write(&config,serde_json::to_vec(&json!({
        "providers":[{"id":"native","base_url":base_url,"protocol":"responses","auth_mode":"forward"}],
        "models":[{"id":"native/alias","provider":"native","upstream_id":"upstream","enabled":true}]
    })).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-test-codex",
        native,
    )
    .unwrap();
    (directory, server)
}

pub(super) fn response_parts(wire: &str) -> (&str, &[u8]) {
    let (head, body) = wire.split_once("\r\n\r\n").expect("response separator");
    (head, body.as_bytes())
}

pub(super) fn masked_websocket_text(value: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(value).unwrap();
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x81];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    frame
}

pub(super) fn send_masked_websocket_text(stream: &mut TcpStream, value: &Value) {
    stream.write_all(&masked_websocket_text(value)).unwrap();
    stream.flush().unwrap();
}

pub(super) fn receive_websocket_json(stream: &mut TcpStream) -> Value {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0] & 0x0f, 1);
    assert_eq!(header[1] & 0x80, 0);
    let mut length = usize::from(header[1] & 0x7f);
    if length == 126 {
        let mut raw = [0u8; 2];
        stream.read_exact(&mut raw).unwrap();
        length = usize::from(u16::from_be_bytes(raw));
    } else if length == 127 {
        let mut raw = [0u8; 8];
        stream.read_exact(&mut raw).unwrap();
        length = usize::try_from(u64::from_be_bytes(raw)).unwrap();
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

pub(super) struct NativeSseUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<ObservedNativeRequest>,
    worker: Option<JoinHandle<()>>,
}

impl NativeSseUpstream {
    pub(super) fn start(ordinary_json: bool) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind native SSE");
        let address = listener.local_addr().unwrap();
        let (sender, observed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept native SSE");
            let raw = read_request_head(&mut stream).unwrap();
            let request = parse_request(&raw.head).unwrap();
            let observed = if request
                .header("Upgrade")
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            {
                stream.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                stream.flush().unwrap();
                drop(stream);
                let (mut stream2, _) = listener.accept().expect("accept HTTP fallback");
                let observed = receive_native_request(&mut stream2);
                stream = stream2;
                observed
            } else {
                finish_native_request(&mut stream, raw)
            };
            sender.send(observed).unwrap();
            if ordinary_json {
                let body=br#"{"id":"resp_json","object":"response","status":"completed","model":"upstream","output":[],"future":"kept"}"#;
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nOpenAI-Model: upstream\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(body).unwrap();
                return;
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nOpenAI-Model: upstream\r\nX-Codex-Turn-State: stream-turn\r\nX-Models-Etag: stale-stream-etag\r\nConnection: close\r\n\r\n").unwrap();
            let wire=concat!(
                "event: response.created\n",
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_stream\",\"status\":\"in_progress\",\"model\":\"upstream\",\"headers\":{\"openai-model\":\"upstream\"}}}\n\n",
                "data: {not-json}\n\n",
                "event: response.output_text.delta\n",
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
                "event: response.completed\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_stream\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"upstream\",\"output\":[],\"headers\":{\"x-openai-model\":\"upstream\"},\"future\":{\"opaque\":true}}}\n\n"
            ).as_bytes();
            for chunk in wire.chunks(17) {
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            address,
            observed,
            worker: Some(worker),
        }
    }
    pub(super) fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    pub(super) fn observed(&self) -> ObservedNativeRequest {
        self.observed.recv_timeout(Duration::from_secs(5)).unwrap()
    }
}
impl Drop for NativeSseUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
