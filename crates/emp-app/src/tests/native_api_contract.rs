use super::*;
use emp_transport::decode_content;
use std::sync::mpsc;

#[derive(Debug)]
struct ObservedNativeRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

fn receive_native_request(stream: &mut TcpStream) -> ObservedNativeRequest {
    let raw = read_request_head(stream).expect("native request head");
    finish_native_request(stream, raw)
}

fn finish_native_request(stream: &mut TcpStream, raw: RequestHead) -> ObservedNativeRequest {
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

struct NativeUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<ObservedNativeRequest>,
    worker: Option<JoinHandle<()>>,
    raw_response: Vec<u8>,
}

impl NativeUpstream {
    fn start(requests: usize) -> Self {
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

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn next(&self) -> ObservedNativeRequest {
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

struct RefreshUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<ObservedNativeRequest>,
    worker: Option<JoinHandle<()>>,
}

impl RefreshUpstream {
    fn start(expected_requests: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind refresh upstream");
        let address = listener.local_addr().expect("refresh upstream address");
        let (sender, observed) = mpsc::sync_channel(expected_requests);
        let worker = thread::spawn(move || {
            for attempt in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept refresh upstream");
                let raw = read_request_head(&mut stream).expect("refresh request head");
                let request = parse_request(&raw.head).expect("refresh HTTP request");
                let headers = request
                    .headers
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| {
                        (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                    })
                    .collect::<BTreeMap<_, _>>();
                let length = headers["content-length"]
                    .parse::<usize>()
                    .expect("Content-Length");
                let mut encoded = raw.body_prefix;
                while encoded.len() < length {
                    let mut chunk = [0u8; 4096];
                    let count = stream.read(&mut chunk).expect("refresh body");
                    assert!(count > 0);
                    encoded.extend_from_slice(&chunk[..count]);
                }
                encoded.truncate(length);
                let decoded =
                    decode_content(encoded, &headers["content-encoding"], 4 * 1024 * 1024, None)
                        .expect("refresh zstd");
                sender
                    .send(ObservedNativeRequest {
                        path: request.target.to_owned(),
                        headers,
                        body: serde_json::from_slice(&decoded).unwrap(),
                    })
                    .unwrap();
                let (status, body) = if attempt == 0 {
                    (401, br#"{"error":{"message":"expired"}}"#.as_slice())
                } else {
                    (200,br#"{"id":"resp_rotated","object":"response","status":"completed","model":"upstream","output":[]}"#.as_slice())
                };
                write!(stream,"HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",status_text(status),body.len()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        Self {
            address,
            observed,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn next(&self) -> ObservedNativeRequest {
        self.observed
            .recv_timeout(Duration::from_secs(5))
            .expect("refresh request")
    }
}

impl Drop for RefreshUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join refresh upstream");
        }
    }
}

fn account_server(
    upstream: &RefreshUpstream,
    codex_binary: &str,
) -> (TempDir, ServerHandle, PathBuf) {
    let directory = tempfile::tempdir().expect("account temp dir");
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().unwrap()).unwrap();
    let mut config = json!({"codex_base_url":upstream.base_url(),"providers":[],"models":[],
        "accounts":[{"id":"egg","name":"Egg","prefix":"egg","enabled":true,"hidden_models":[]}]});
    let auth_path = emp_state::account_auth_path(&config, "egg", &config_path).unwrap();
    config["accounts"][0]["auth_file"] = Value::String(auth_path.to_string_lossy().into_owned());
    std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        codex_binary,
        native_auth_path,
    )
    .expect("start account EMP");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"original-secret","account_id":"selected-owner"}}),
        )
        .unwrap();
    (directory, server, auth_path)
}

struct ScenarioUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<ObservedNativeRequest>,
    worker: Option<JoinHandle<()>>,
}

impl ScenarioUpstream {
    fn start(case: &'static str, expected_requests: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind scenario upstream");
        let address = listener.local_addr().unwrap();
        let (sender, observed) = mpsc::sync_channel(expected_requests);
        let worker = thread::spawn(move || {
            for attempt in 0..expected_requests {
                let (mut stream, _) = listener.accept().expect("accept scenario upstream");
                let request = receive_native_request(&mut stream);
                let has_effort = request.body.get("reasoning_effort").is_some();
                sender.send(request).unwrap();
                if case == "network" && attempt == 0 {
                    continue;
                }
                let (status,body)=match case {
                    "reasoning" if has_effort => (400,br#"{"error":{"message":"unknown field reasoning_effort"}}"#.as_slice()),
                    "rate" => (429,br#"{"error":{"message":"rate limited"}}"#.as_slice()),
                    "gateway" => (504,br#"{"error":{"message":"gateway timeout"}}"#.as_slice()),
                    "context" => (400,br#"{"error":{"code":"context_length_exceeded","message":"maximum context length exceeded"}}"#.as_slice()),
                    "forward401" => (401,br#"{"error":{"message":"unauthorized"}}"#.as_slice()),
                    _ => (200,br#"{"id":"resp_scenario","object":"response","status":"completed","model":"upstream","output":[]}"#.as_slice()),
                };
                let retry = if case == "rate" {
                    "Retry-After: 1.2\r\n"
                } else {
                    ""
                };
                write!(stream,"HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{retry}Connection: close\r\n\r\n",status_text(status),body.len()).unwrap();
                stream.write_all(body).unwrap();
            }
        });
        Self {
            address,
            observed,
            worker: Some(worker),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn requests(&self, count: usize) -> Vec<ObservedNativeRequest> {
        (0..count)
            .map(|_| {
                self.observed
                    .recv_timeout(Duration::from_secs(5))
                    .expect("scenario request")
            })
            .collect()
    }
}
impl Drop for ScenarioUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join scenario upstream");
        }
    }
}

fn forward_server(base_url: &str) -> (TempDir, ServerHandle) {
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

struct NativeSseUpstream {
    address: SocketAddr,
    observed: mpsc::Receiver<ObservedNativeRequest>,
    worker: Option<JoinHandle<()>>,
}

impl NativeSseUpstream {
    fn start(ordinary_json: bool) -> Self {
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
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn observed(&self) -> ObservedNativeRequest {
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

struct NativeErrorSseUpstream {
    address: SocketAddr,
    worker: Option<JoinHandle<()>>,
}
impl NativeErrorSseUpstream {
    fn start(wire: Vec<u8>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = receive_native_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            for chunk in wire.chunks(13) {
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            address,
            worker: Some(worker),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}
impl Drop for NativeErrorSseUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

fn native_alias_server(base_url: &str) -> (TempDir, ServerHandle) {
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

fn response_parts(wire: &str) -> (&str, &[u8]) {
    let (head, body) = wire.split_once("\r\n\r\n").expect("response separator");
    (head, body.as_bytes())
}

#[test]
fn native_responses_endpoint_forwards_zstd_owner_credentials_and_codex_metadata() {
    let upstream = NativeUpstream::start(2);
    let directory = tempfile::tempdir().expect("native temp dir");
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().unwrap()).expect("native directory");
    let mut config = json!({
        "codex_base_url":upstream.base_url(),
        "providers":[{
            "id":"native-forward","name":"Native Forward","base_url":upstream.base_url(),
            "protocol":"responses","auth_mode":"forward"
        }],
        "accounts":[{
            "id":"egg","name":"Egg","prefix":"egg",
            "enabled":true,"hidden_models":[]
        }],
        "models":[]
    });
    let auth_path = emp_state::account_auth_path(&config, "egg", &config_path)
        .expect("managed account auth path");
    config["accounts"][0]["auth_file"] = Value::String(auth_path.to_string_lossy().into_owned());
    std::fs::create_dir_all(auth_path.parent().unwrap()).expect("account directory");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).expect("native config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "missing-test-codex",
        native_auth_path,
    )
    .expect("start Rust EMP");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"selected-secret","account_id":"selected-owner"}}),
        )
        .expect("encrypted account fixture");

    let body = |model: &str| {
        serde_json::to_vec(&json!({
            "model":model,"input":"hello","stream":false,
            "future_request_field":{"opaque":true}
        }))
        .unwrap()
    };
    let cookie = session_cookie_header(&server);
    let context = [
        "Authorization: Bearer caller-secret",
        "chatgpt-account-id: caller-owner",
        "thread-id: thread-fixture",
        "x-openai-subagent: subagent-fixture",
        &cookie,
    ];
    let forward = post(&server, "/v1/responses", &body("upstream"), &context);
    let account = post(&server, "/v1/responses", &body("egg/upstream"), &context);

    for wire in [&forward, &account] {
        let (head, returned) = response_parts(wire);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert_eq!(returned, upstream.raw_response);
        assert!(head.contains("x-codex-turn-state: fixture-turn\r\n"));
        let etag = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("x-models-etag: ")
                    .map(str::to_owned)
            })
            .expect("catalog ETag");
        assert_ne!(etag, "stale-upstream-etag");
    }
    assert!(
        response_parts(&forward)
            .0
            .contains("openai-model: upstream\r\n")
    );
    assert!(
        response_parts(&account)
            .0
            .contains("openai-model: egg/upstream\r\n")
    );

    let forward = upstream.next();
    let account = upstream.next();
    for observed in [&forward, &account] {
        assert_eq!(observed.path, "/v1/responses");
        assert_eq!(observed.headers["content-encoding"], "zstd");
        assert_eq!(observed.headers["thread-id"], "thread-fixture");
        assert_eq!(observed.headers["x-openai-subagent"], "subagent-fixture");
        assert_eq!(observed.body["model"], "upstream");
        assert_eq!(observed.body["future_request_field"]["opaque"], true);
    }
    assert_eq!(forward.headers["authorization"], "Bearer caller-secret");
    assert_eq!(forward.headers["chatgpt-account-id"], "caller-owner");
    assert_eq!(account.headers["authorization"], "Bearer selected-secret");
    assert_eq!(account.headers["chatgpt-account-id"], "selected-owner");
    server.shutdown().expect("shutdown Rust EMP");
}

#[cfg(unix)]
#[test]
fn native_account_401_refreshes_once_and_failed_refresh_keeps_original_rejection() {
    use std::os::unix::fs::PermissionsExt;

    let successful = RefreshUpstream::start(2);
    let target = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target");
    std::fs::create_dir_all(&target).unwrap();
    let script_root = tempfile::Builder::new()
        .prefix("emp-native-endpoint-codex-")
        .tempdir_in(target)
        .expect("fake Codex root");
    let script = script_root.path().join("fake-codex");
    std::fs::write(&script,r#"#!/usr/bin/env python3
import json, os, pathlib, sys
home=pathlib.Path(os.environ['CODEX_HOME'])
assert sys.argv[1:]==['app-server','--stdio']
for line in sys.stdin:
    request=json.loads(line); method=request.get('method')
    if method=='initialize': print(json.dumps({'id':request['id'],'result':{}}),flush=True)
    elif method=='initialized': pass
    elif method=='account/read':
        auth=json.loads((home/'auth.json').read_text()); auth['tokens']['access_token']='rotated-secret'; (home/'auth.json').write_text(json.dumps(auth))
        print(json.dumps({'id':request['id'],'result':{'account':{'email':'xian@example.com','planType':'pro'}}}),flush=True)
    elif method=='account/rateLimits/read':
        print(json.dumps({'id':request['id'],'result':{'rateLimits':{'limitId':'codex','primary':{'usedPercent':7}}}}),flush=True)
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (_directory, server, auth_path) = account_server(&successful, script.to_str().unwrap());
    let cookie = session_cookie_header(&server);
    let body = serde_json::to_vec(&json!({"model":"egg/upstream","input":"hello","stream":false}))
        .unwrap();
    let response = post(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let first = successful.next();
    let second = successful.next();
    assert_eq!(first.headers["authorization"], "Bearer original-secret");
    assert_eq!(second.headers["authorization"], "Bearer rotated-secret");
    assert_eq!(
        first.body, second.body,
        "retry must reuse one projected request"
    );
    assert_eq!(
        server
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&auth_path)
            .unwrap()["tokens"]["access_token"],
        "rotated-secret"
    );
    server.shutdown().unwrap();

    let failed = RefreshUpstream::start(1);
    let (_directory, server, _) = account_server(&failed, "missing-test-codex");
    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    assert!(
        response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{response}"
    );
    assert_eq!(
        failed.next().headers["authorization"],
        "Bearer original-secret"
    );
    server.shutdown().unwrap();
}

#[test]
fn native_endpoint_matches_retry_and_terminal_error_decisions() {
    for (case, expected_status, attempts) in [
        ("reasoning", 200, 2usize),
        ("rate", 429, 1),
        ("gateway", 504, 1),
        ("context", 413, 1),
        ("forward401", 401, 1),
        ("network", 200, 2),
    ] {
        let upstream = ScenarioUpstream::start(case, attempts);
        let (_directory, server) = forward_server(&upstream.base_url());
        let cookie = session_cookie_header(&server);
        let mut body = json!({"model":"upstream","input":"hello","stream":false});
        if case == "reasoning" {
            body["reasoning_effort"] = json!("low");
        }
        let wire = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&body).unwrap(),
            &[&cookie, "Authorization: Bearer caller"],
        );
        let status: u16 = wire.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(status, expected_status, "scenario {case}: {wire}");
        if case == "rate" {
            assert!(response_parts(&wire).0.contains("Retry-After: 2\r\n"));
        }
        let requests = upstream.requests(attempts);
        if case == "reasoning" {
            assert!(requests[0].body.get("reasoning_effort").is_some());
            assert!(requests[1].body.get("reasoning_effort").is_none());
            let mut first = requests[0].body.clone();
            first.as_object_mut().unwrap().remove("reasoning_effort");
            assert_eq!(
                first, requests[1].body,
                "fallback changes only reasoning_effort"
            );
        } else if attempts == 2 {
            assert_eq!(
                requests[0].body, requests[1].body,
                "network retry reuses projected body"
            );
        }
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_and_ordinary_json_cross_the_real_endpoint() {
    for ordinary in [false, true] {
        let upstream = NativeSseUpstream::start(ordinary);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_cookie_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let wire = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[
                &cookie,
                "Authorization: Bearer caller",
                "thread-id: stream-thread",
                "x-openai-subagent: stream-subagent",
            ],
        );
        assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"), "{wire}");
        let (head, events) = wire.split_once("\r\n\r\n").unwrap();
        assert!(head.contains("openai-model: native/alias\r\n"), "{head}");
        if !ordinary {
            assert!(head.contains("x-codex-turn-state: stream-turn\r\n"));
            assert!(!head.contains("stale-stream-etag"));
            assert!(events.contains("response.created"));
            assert!(events.contains("response.output_text.delta"));
            assert!(events.contains("response.completed"));
            assert!(!events.contains("not-json"));
            assert!(events.contains("\"openai-model\":\"native/alias\""));
            assert!(events.contains("\"x-openai-model\":\"native/alias\""));
            assert!(events.contains("\"model\":\"upstream\""));
            assert!(events.contains("\"future\":{\"opaque\":true}"));
        } else {
            assert!(events.contains("response.created"));
            assert!(events.contains("response.completed"));
            assert!(events.contains("\"future\":\"kept\""));
        }
        let observed = upstream.observed();
        assert_eq!(observed.headers["content-encoding"], "zstd");
        assert_eq!(observed.headers["authorization"], "Bearer caller");
        assert_eq!(observed.headers["thread-id"], "stream-thread");
        assert_eq!(observed.headers["x-openai-subagent"], "stream-subagent");
        assert_eq!(observed.body["model"], "upstream");
        assert_eq!(observed.body["stream"], true);
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_context_and_incomplete_boundaries_match_codex_http_behavior() {
    let cases=[
        ("context",b"data: {\"type\":\"error\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"maximum context length exceeded\"}}\n\n".to_vec(),413,false),
        ("pre_incomplete",b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_x\",\"status\":\"in_progress\"}}\n\n".to_vec(),502,false),
        ("post_incomplete",b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_x\",\"status\":\"in_progress\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".to_vec(),200,true),
        ("failed",b"data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_x\",\"status\":\"failed\",\"error\":{\"status\":429,\"error_class\":\"rate_limit\",\"code\":\"rate_limit_exceeded\",\"retry_after_seconds\":3}}}\n\n".to_vec(),429,false),
    ];
    for (name, wire, expected, streamed) in cases {
        let upstream = NativeErrorSseUpstream::start(wire);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_cookie_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let response = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&cookie, "Authorization: Bearer caller"],
        );
        let status: u16 = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(status, expected, "{name}: {response}");
        if streamed {
            assert!(response.contains("response.output_text.delta"));
            assert!(response.contains("response.failed"));
            assert!(response.contains("stream_incomplete"));
        }
        if name == "failed" {
            assert!(response.contains("Retry-After: 3\r\n"));
        }
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_downstream_disconnect_cancels_upstream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (head_sender, head_ready) = mpsc::sync_channel(1);
    let (closed_sender, closed) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = receive_native_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream.flush().unwrap();
        head_sender.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut byte = [0u8; 1];
        let ended = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        closed_sender.send(ended).unwrap();
    });
    let (_directory, server) = native_alias_server(&format!("http://{address}/v1"));
    let cookie = session_cookie_header(&server);
    let body =
        serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true})).unwrap();
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    head_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(downstream);
    assert!(
        closed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "Rust EMP retained native upstream after Codex disconnected"
    );
    server.shutdown().unwrap();
    worker.join().unwrap();
}

fn send_masked_websocket_text(stream: &mut TcpStream, value: &Value) {
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
    stream.write_all(&frame).unwrap();
    stream.flush().unwrap();
}

fn receive_websocket_json(stream: &mut TcpStream) -> Value {
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

#[test]
fn responses_websocket_keeps_connection_and_requests_full_recovery_for_missing_previous() {
    let upstream = NativeSseUpstream::start(false);
    let (_directory, server) = native_alias_server(&upstream.base_url());
    let cookie = session_cookie_header(&server);
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream,"GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\nAuthorization: Bearer caller\r\nthread-id: websocket-thread\r\n\r\n",server.local_addr().port()).unwrap();
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
    send_masked_websocket_text(
        &mut stream,
        &json!({"type":"response.create","model":"native/alias","input":"hello"}),
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
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "response.completed"
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
fn native_compact_endpoint_preserves_opaque_response_and_owned_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, observed) = mpsc::sync_channel(1);
    let raw=br#"{ "type":"compaction", "encrypted_content":"opaque-ciphertext", "future":{"kept":true} }"#.to_vec();
    let returned = raw.clone();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        sender.send(receive_native_request(&mut stream)).unwrap();
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nOpenAI-Model: upstream\r\nX-Models-Etag: stale-compact\r\nConnection: close\r\n\r\n",returned.len()).unwrap();
        stream.write_all(&returned).unwrap();
    });
    let (_directory, server) = native_alias_server(&format!("http://{address}/v1"));
    let cookie = session_cookie_header(&server);
    let body=serde_json::to_vec(&json!({"model":"native/alias","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"history"}]}]})).unwrap();
    let response = post(
        &server,
        "/v1/responses/compact",
        &body,
        &[
            &cookie,
            "Authorization: Bearer caller",
            "thread-id: compact-thread",
        ],
    );
    let (head, body) = response_parts(&response);
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(body, raw);
    assert!(head.contains("openai-model: native/alias\r\n"));
    assert!(!head.contains("stale-compact"));
    let request = observed.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(request.path, "/v1/responses/compact");
    assert_eq!(request.headers["content-encoding"], "zstd");
    assert_eq!(request.headers["thread-id"], "compact-thread");
    assert_eq!(request.body["model"], "upstream");
    server.shutdown().unwrap();
    worker.join().unwrap();
}

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
    let cookie = session_cookie_header(&server);
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
        let cookie = session_cookie_header(&server);
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
            Ok((stream, _)) => return Some(stream),
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
    let cookie = session_cookie_header(&server);
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
