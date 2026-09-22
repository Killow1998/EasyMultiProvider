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
