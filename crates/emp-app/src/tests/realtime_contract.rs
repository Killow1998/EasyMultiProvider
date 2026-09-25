use super::*;
use std::process::Command;

#[derive(Clone, Copy)]
pub(super) struct UpstreamResponse {
    pub(super) status: u16,
    pub(super) reason: &'static str,
    pub(super) content_type: &'static str,
    pub(super) location: Option<&'static str>,
    pub(super) body: &'static [u8],
}

struct UpstreamRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

pub(super) struct RealtimeUpstream {
    address: SocketAddr,
    request: mpsc::Receiver<UpstreamRequest>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RealtimeUpstream {
    pub(super) fn start(response: UpstreamResponse) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind Voice upstream");
        listener
            .set_nonblocking(true)
            .expect("nonblocking Voice upstream");
        let address = listener.local_addr().expect("Voice upstream address");
        let (sender, request) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(true)
                            .expect("nonblocking accepted Voice stream");
                        stream
                            .set_nonblocking(false)
                            .expect("blocking accepted Voice stream");
                        let observed = receive_request(&mut stream);
                        sender.send(observed).expect("record Voice call");
                        let location = response
                            .location
                            .map(|value| format!("Location: {value}\r\n"))
                            .unwrap_or_default();
                        write!(
                            stream,
                            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n",
                            response.status,
                            response.reason,
                            response.content_type,
                            response.body.len()
                        )
                        .expect("write Voice response headers");
                        stream
                            .write_all(response.body)
                            .expect("write Voice response body");
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            request,
            stop,
            worker: Some(worker),
        }
    }

    pub(super) fn base_url(&self) -> String {
        format!("http://{}/backend", self.address)
    }

    fn observed(&self) -> UpstreamRequest {
        self.request
            .recv_timeout(Duration::from_secs(5))
            .expect("upstream call received")
    }

    pub(super) fn no_request(&self) -> bool {
        matches!(self.request.try_recv(), Err(mpsc::TryRecvError::Empty))
    }
}

impl Drop for RealtimeUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join Voice fake upstream");
        }
    }
}

fn receive_request(stream: &mut TcpStream) -> UpstreamRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("upstream read timeout");
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let (separator, body_length) = loop {
        let count = stream.read(&mut buffer).expect("read upstream request");
        assert!(count > 0, "upstream request ended before headers/body");
        raw.extend_from_slice(&buffer[..count]);
        let Some(separator) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&raw[..separator]).expect("ASCII request headers");
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .expect("upstream Content-Length");
        if raw.len() >= separator + 4 + length {
            break (separator, length);
        }
    };
    let head = std::str::from_utf8(&raw[..separator]).expect("ASCII request head");
    let mut lines = head.lines();
    let mut request_line = lines
        .next()
        .expect("upstream request line")
        .split_whitespace();
    let method = request_line.next().unwrap().to_owned();
    let path = request_line.next().unwrap().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let body_start = separator + 4;
    UpstreamRequest {
        method,
        path,
        headers,
        body: raw[body_start..body_start + body_length].to_vec(),
    }
}

fn multipart_offer() -> Vec<u8> {
    let boundary = "VoiceFixtureBoundary";
    format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"sdp\"\r\nContent-Type: application/sdp\r\n\r\nv=0\r\no=offer\r\n\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"session\"\r\nContent-Type: application/json\r\n\r\n{{\"model\":\"gpt-live\",\"delegation\":{{\"type\":\"client\"}}}}\r\n--{boundary}--\r\n"
    )
    .into_bytes()
}

fn response_parts(raw: &str) -> (u16, BTreeMap<String, String>, Vec<u8>) {
    let separator = raw.find("\r\n\r\n").expect("HTTP response separator");
    let headers = &raw[..separator];
    let mut lines = headers.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .expect("HTTP status");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (status, headers, raw.as_bytes()[separator + 4..].to_vec())
}

fn selected_headers(request: &UpstreamRequest) -> Value {
    let selected = [
        "authorization",
        "chatgpt-account-id",
        "openai-alpha",
        "session-id",
        "thread-id",
        "content-type",
        "accept",
        "user-agent",
    ];
    let mut result = serde_json::Map::new();
    for name in selected {
        if let Some(value) = request.headers.get(name) {
            result.insert(name.to_owned(), json!(value));
        }
    }
    Value::Object(result)
}

fn error_code(raw: &str) -> String {
    let (status, _, body) = response_parts(raw);
    assert_ne!(status, 200);
    serde_json::from_slice::<Value>(&body).unwrap()["error"]["code"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub(super) fn app_server_for(
    upstream: &RealtimeUpstream,
    native_auth: &Path,
) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let app_config = canonical_root(&directory).join("config.json");
    std::fs::write(
        &app_config,
        serde_json::to_vec(&json!({
            "codex_base_url":upstream.base_url(),
            "providers":[],
            "models":[]
        }))
        .expect("encode fixture config"),
    )
    .expect("write fixture config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &app_config,
        "missing-test-codex",
        native_auth.to_path_buf(),
    )
    .expect("start realtime fixture server");
    (directory, server)
}

#[test]
fn live_call_matches_official_python_forwarding_over_real_http() {
    let Some(python) = std::env::var_os("EMP_PYTHON_INTEROP") else {
        return;
    };
    let oracle_root = std::env::var_os("EMP_PYTHON_ORACLE_ROOT")
        .expect("EMP_PYTHON_ORACLE_ROOT must name official Python oracle");
    let oracle_root = PathBuf::from(oracle_root);
    assert!(
        oracle_root
            .join("easy_multi_provider/realtime.py")
            .is_file()
    );
    let directory = tempfile::tempdir().expect("fixture root");
    let native_auth = canonical_root(&directory).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    std::fs::write(
        &native_auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .expect("write native auth");

    let python_upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 201,
        reason: "Created",
        content_type: "application/sdp; charset=utf-8",
        location: Some("/v1/live/rtc_voice_123"),
        body: b"v=answer\r\n",
    });
    let script = format!(
        r#"
import json, sys
from pathlib import Path
import easy_multi_provider.realtime as realtime
realtime.__version__ = "{}"
from easy_multi_provider.realtime import RealtimeCall, forward_native_realtime_call
result = forward_native_realtime_call(
    sys.argv[1], Path(sys.argv[2]), {{
        "Authorization":"Bearer caller-secret", "OpenAI-Alpha":"quicksilver=v2",
        "Session-Id":"session-voice", "Thread-Id":"thread-voice",
        "X-Ignored-Secret":"must-not-forward",
    }}, RealtimeCall("v=0\r\no=offer\r\n", {{"model":"gpt-live","delegation":{{"type":"client"}}}}))
print(json.dumps({{"status":result.status,"content_type":result.content_type,
    "location":result.location,"body":result.body.decode("utf-8")}}))
"#,
        env!("CARGO_PKG_VERSION")
    );
    let oracle = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(python_upstream.base_url())
        .arg(&native_auth)
        .current_dir(oracle_root)
        .output()
        .expect("run official Python realtime oracle");
    assert!(
        oracle.status.success(),
        "Python Voice oracle failed: {}",
        String::from_utf8_lossy(&oracle.stderr)
    );
    let expected: Value = serde_json::from_slice(&oracle.stdout).expect("Python oracle JSON");
    let python_request = python_upstream.observed();

    let rust_upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 201,
        reason: "Created",
        content_type: "application/sdp; charset=utf-8",
        location: Some("/v1/live/rtc_voice_123"),
        body: b"v=answer\r\n",
    });
    let (_app_directory, server) = app_server_for(&rust_upstream, &native_auth);
    let body = multipart_offer();
    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/v1/live",
        &body,
        &[
            &cookie,
            "Authorization: Bearer caller-secret",
            "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary",
            "OpenAI-Alpha: quicksilver=v2",
            "Session-Id: session-voice",
            "Thread-Id: thread-voice",
            "X-Ignored-Secret: must-not-forward",
        ],
    );
    let (status, headers, response_body) = response_parts(&response);
    let actual = json!({
        "status":status,
        "content_type":headers["content-type"],
        "location":headers["location"],
        "body":String::from_utf8(response_body).unwrap()
    });
    assert_eq!(actual, expected);
    let rust_request = rust_upstream.observed();
    assert_eq!(rust_request.method, "POST");
    assert_eq!(
        rust_request.path,
        "/backend/realtime/calls?intent=quicksilver&architecture=avas"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&rust_request.body).unwrap(),
        serde_json::from_slice::<Value>(&python_request.body).unwrap()
    );
    assert_eq!(
        selected_headers(&rust_request),
        selected_headers(&python_request)
    );
    assert_eq!(
        rust_request.headers["authorization"],
        "Bearer native-secret"
    );
    assert_eq!(rust_request.headers["chatgpt-account-id"], "acct-native");
    assert!(!rust_request.headers.contains_key("x-ignored-secret"));
    server.shutdown().expect("shutdown realtime fixture server");
}

#[test]
fn live_call_preserves_non_success_redirect_without_retry() {
    let directory = tempfile::tempdir().expect("fixture root");
    let native_auth = canonical_root(&directory).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    std::fs::write(
        &native_auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .expect("write native auth");
    let upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 307,
        reason: "Temporary Redirect",
        content_type: "text/plain",
        location: Some("/voice-temporarily-unavailable"),
        body: b"retry elsewhere",
    });
    let (_app_directory, server) = app_server_for(&upstream, &native_auth);
    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/v1/live",
        &multipart_offer(),
        &[
            &cookie,
            "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary",
        ],
    );
    let (status, headers, body) = response_parts(&response);
    assert_eq!(status, 307);
    assert_eq!(headers["content-type"], "text/plain");
    assert_eq!(headers["location"], "/voice-temporarily-unavailable");
    assert_eq!(body, b"retry elsewhere");
    let observed = upstream.observed();
    assert_eq!(
        observed.path,
        "/backend/realtime/calls?intent=quicksilver&architecture=avas"
    );
    server.shutdown().expect("shutdown realtime fixture server");
}

#[test]
fn live_call_preserves_upstream_auth_error_and_clarifies_empty_unsupported_error() {
    for (response_spec, expected_status, expected_body, expected_code) in [
        (
            UpstreamResponse {
                status: 401,
                reason: "Unauthorized",
                content_type: "application/problem+json",
                location: None,
                body: br#"{"error":{"message":"expired"}}"#,
            },
            401,
            br#"{"error":{"message":"expired"}}"#.as_slice(),
            None,
        ),
        (
            UpstreamResponse {
                status: 404,
                reason: "Not Found",
                content_type: "application/json",
                location: None,
                body: b"",
            },
            404,
            br#"{"error":{"code":"native_realtime_unsupported","message":"The native subscription backend does not support Codex Voice"}}"#.as_slice(),
            Some("native_realtime_unsupported"),
        ),
    ] {
        let directory = tempfile::tempdir().expect("fixture root");
        let native_auth = canonical_root(&directory).join("codex/auth.json");
        std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
        std::fs::write(
            &native_auth,
            br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
        )
        .expect("write native auth");
        let upstream = RealtimeUpstream::start(response_spec);
        let (_app_directory, server) = app_server_for(&upstream, &native_auth);
        let cookie = session_cookie_header(&server);
        let response = post(
            &server,
            "/v1/live",
            &multipart_offer(),
            &[
                &cookie,
                "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary",
            ],
        );
        let (status, headers, body) = response_parts(&response);
        assert_eq!(status, expected_status);
        assert_eq!(headers["content-type"], response_spec.content_type);
        assert_eq!(body, expected_body);
        if let Some(code) = expected_code {
            assert_eq!(error_code(&response), code);
        }
        let observed = upstream.observed();
        assert_eq!(observed.path, "/backend/realtime/calls?intent=quicksilver&architecture=avas");
        server.shutdown().expect("shutdown realtime fixture server");
    }
}

#[test]
fn live_success_requires_location_before_sideband_can_start() {
    let directory = tempfile::tempdir().expect("fixture root");
    let native_auth = canonical_root(&directory).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    std::fs::write(
        &native_auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .expect("write native auth");
    let upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 200,
        reason: "OK",
        content_type: "application/sdp",
        location: None,
        body: b"v=answer\r\n",
    });
    let (_app_directory, server) = app_server_for(&upstream, &native_auth);
    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/v1/live",
        &multipart_offer(),
        &[
            &cookie,
            "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary",
        ],
    );
    let (status, _, body) = response_parts(&response);
    assert_eq!(status, 502);
    assert_eq!(error_code(&response), "realtime_upstream_invalid");
    let error: Value = serde_json::from_slice(&body).expect("safe error JSON");
    assert_eq!(
        error["error"]["message"],
        "Native realtime response is missing a valid call Location"
    );
    let observed = upstream.observed();
    assert_eq!(
        observed.path,
        "/backend/realtime/calls?intent=quicksilver&architecture=avas"
    );
    server.shutdown().expect("shutdown realtime fixture server");
}

#[test]
fn live_caller_auth_is_checked_before_upstream_connect() {
    let directory = tempfile::tempdir().expect("fixture root");
    let native_auth = canonical_root(&directory).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    std::fs::write(
        &native_auth,
        br#"{"tokens":{"access_token":"native-secret","account_id":"acct-native"}}"#,
    )
    .expect("write native auth");
    let upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 200,
        reason: "OK",
        content_type: "application/sdp",
        location: Some("/v1/live/rtc_never_called"),
        body: b"v=answer",
    });
    let (_app_directory, server) = app_server_for(&upstream, &native_auth);
    let response = post(
        &server,
        "/v1/live",
        &multipart_offer(),
        &[
            "Authorization: Bearer wrong-caller",
            "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary",
        ],
    );
    let (status, _, _) = response_parts(&response);
    assert_eq!(status, 401);
    assert_eq!(error_code(&response), "realtime_caller_unauthorized");
    assert!(matches!(
        upstream.request.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    server.shutdown().expect("shutdown realtime fixture server");
}

#[test]
fn live_reports_missing_native_auth_after_caller_authentication() {
    let directory = tempfile::tempdir().expect("fixture root");
    let native_auth = canonical_root(&directory).join("codex/auth.json");
    std::fs::create_dir_all(native_auth.parent().unwrap()).expect("native auth directory");
    let upstream = RealtimeUpstream::start(UpstreamResponse {
        status: 200,
        reason: "OK",
        content_type: "application/sdp",
        location: Some("/v1/live/rtc_never_called"),
        body: b"v=answer",
    });
    let (_app_directory, server) = app_server_for(&upstream, &native_auth);
    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/v1/live",
        &multipart_offer(),
        &[
            &cookie,
            "Content-Type: multipart/form-data; boundary=VoiceFixtureBoundary",
        ],
    );
    let (status, _, _) = response_parts(&response);
    assert_eq!(status, 401);
    assert_eq!(error_code(&response), "native_subscription_unavailable");
    assert!(matches!(
        upstream.request.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    server.shutdown().expect("shutdown realtime fixture server");
}
