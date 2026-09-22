use super::*;

pub(super) struct CatalogUpstream {
    address: SocketAddr,
    requests: mpsc::Receiver<(String, BTreeMap<String, String>, Value)>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl CatalogUpstream {
    pub(super) fn start(status: u16) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("discovery listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("address");
        let (sender, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        sender
                            .send(receive_upstream_request(&mut stream))
                            .expect("record request");
                        let payload = if status == 200 {
                            json!({"data":[
                                {"id":"new", "name":"New model", "context_length":128000, "supported_parameters":["tools","reasoning_effort"], "reasoning_levels":["low","high"]},
                                {"id":"old", "context_length":64000}
                            ]})
                        } else {
                            json!({"error":{"message":"synthetic private upstream diagnostic"}})
                        };
                        let body = serde_json::to_vec(&payload).expect("body");
                        write!(stream,"HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",status_text(status),body.len()).expect("head");
                        stream.write_all(&body).expect("body");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("discovery accept: {error}"),
                }
            }
        });
        Self {
            address,
            requests,
            stop,
            worker: Some(worker),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn observed(&self) {
        let (path, headers, body) = self
            .requests
            .recv_timeout(Duration::from_secs(5))
            .expect("discovery request");
        assert_eq!(path, "/v1/models");
        assert_eq!(headers["authorization"], "Bearer synthetic-test-key");
        assert_eq!(body, Value::Null);
    }
}
impl Drop for CatalogUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join discovery server");
        }
    }
}
pub(super) fn catalog_server(upstream: &CatalogUpstream) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native_path = root.join("codex").join("models_cache.json");
    std::fs::create_dir_all(native_path.parent().expect("parent")).expect("native dir");
    std::fs::write(
        &native_path,
        serde_json::to_vec(&json!({"models":[{
            "slug":"native", "display_name":"Native Model", "context_window":100000,
            "max_context_window":200000, "visibility":"list", "base_instructions":"coding"
        }]}))
        .expect("native JSON"),
    )
    .expect("write native fixture");
    std::fs::write(&config_path,serde_json::to_vec(&json!({
        "native_catalog_path":native_path,
        "providers":[{"id":"demo","base_url":upstream.base_url(),"protocol":"chat_completions","api_key":"synthetic-test-key"}],
        "models":[{"id":"demo/old","provider":"demo","upstream_id":"old","context_window":77777}]
    })).expect("config JSON")).expect("write config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "codex",
        root.join("codex").join("auth.json"),
    )
    .expect("server");
    (directory, server)
}
pub(super) fn parsed_body(wire: &str) -> Value {
    serde_json::from_str(wire.split_once("\r\n\r\n").expect("response head").1).expect("JSON")
}

#[test]
fn catalog_http_contract_matches_live_python_handler() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let cookie = session_cookie_header(&server);
    let cases = json!([
        {"path":"/v1/models"},
        {"path":"/v1/models?client_version="},
        {"path":"/v1/models?client_version=0.155.0"},
        {"path":"/v1/models/demo%2Fold"},
        {"path":"/v1/models/missing"},
        {"path":"/api/providers/discover", "body":{"provider":"demo"}},
        {"path":"/api/providers/discover", "body":{}},
        {"path":"/api/providers/discover", "body":{"provider":"absent"}},
        {"path":"/api/providers/discover", "body":{"provider":"demo", "selected":false}},
        {"path":"/api/providers/discover", "body":{"provider":"demo", "selected":["missing"]}},
        {"path":"/api/catalog/refresh", "body":{}},
        {"path":"/api/config"},
        {"path":"/api/accounts/%40native/models"},
        {"path":"/api/accounts/absent/models"},
        {"path":"/api/accounts/models"},
        {"path":"/api/accounts//models"}
    ]);
    let actual = cases
        .as_array()
        .expect("cases")
        .iter()
        .map(|case| {
            let path = case["path"].as_str().expect("path");
            let wire = if let Some(body) = case.get("body") {
                post(
                    &server,
                    path,
                    &serde_json::to_vec(body).expect("request JSON"),
                    &[&cookie],
                )
            } else if path.starts_with("/api/") {
                request(&server, path, &[&cookie])
            } else {
                request(&server, path, &[])
            };
            let status: u16 = wire
                .split_whitespace()
                .nth(1)
                .expect("status")
                .parse()
                .expect("status number");
            let etag = wire
                .split("\r\n\r\n")
                .next()
                .expect("headers")
                .lines()
                .find_map(|line| line.strip_prefix("ETag: "));
            json!({"status":status, "payload":parsed_body(&wire), "etag":etag})
        })
        .collect::<Vec<_>>();
    let fixture = json!({
        "config":server.state.backend.configuration.config.lock().expect("config").clone(),
        "catalog_path":canonical_root(&directory).join("codex/easy-multi-provider/catalog.json"),
        "discovered":actual[5]["payload"]["models"], "cases":cases,
    });
    let script = r#"
import json, os, sys, threading
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from easy_multi_provider import server

fixture = json.load(sys.stdin)
state = server.AppState.__new__(server.AppState)
state.config = fixture['config']
state.lock = threading.RLock()
state.discovery_lock = threading.Lock()
state._catalog_cache = None
state._catalog_cache_revision = None
state.codex_home = Path(os.environ['CODEX_HOME'])
state._native_quota = None
state.integration_catalog_path = Path(fixture['catalog_path'])
state.integration_status = lambda: SimpleNamespace(state='inactive')
results = []
with patch.object(server, 'discover_models', return_value=fixture['discovered']):
    for case in fixture['cases']:
        handler = object.__new__(server.make_handler(state))
        handler.path = case['path']
        handler.headers = {}
        handler._record_http_request_start_once = lambda: None
        handler._record_management_event = lambda *args, **kwargs: None
        handler._management_allowed = lambda: True
        captured = {}
        def send(status, data, *args, **kwargs):
            captured.update(status=status, payload=json.loads(data), etag=kwargs.get('headers', {}).get('ETag'))
        handler._send = send
        if 'body' in case:
            handler._body = lambda _limit: case['body']
            handler._do_POST()
        else:
            handler.do_GET()
        results.append(captured)
json.dump(results, sys.stdout, ensure_ascii=False)
"#;
    let mut child = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .env("CODEX_HOME", canonical_root(&directory).join("codex"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("Python HTTP oracle");
    serde_json::to_writer(child.stdin.take().expect("stdin"), &fixture).expect("oracle input");
    let output = child.wait_with_output().expect("oracle result");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    assert_eq!(json!(actual), expected);
    server.shutdown().expect("shutdown server");
}

#[test]
fn management_body_limits_match_python_before_and_after_decompression() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import base64, gzip, io, json, sys
from email.message import Message
from types import SimpleNamespace
from easy_multi_provider.server import make_handler
limit = 5 * 1024 * 1024
payload = b'{"padding":"' + b'x' * limit + b'"}'
cases = []
for data, size, encoding in [(b'',len(payload),''), (gzip.compress(payload), None, 'gzip')]:
    handler = object.__new__(make_handler(SimpleNamespace()))
    handler.path = '/api/catalog/refresh'
    handler.headers = Message()
    handler.headers['Content-Type'] = 'application/json'
    handler.headers['Content-Length'] = str(size if size is not None else len(data))
    handler.headers['Content-Encoding'] = encoding
    handler.rfile = io.BytesIO(data)
    handler._management_allowed = lambda: True
    handler._record_http_request_start_once = lambda: None
    handler._record_management_event = lambda *args, **kwargs: None
    handler._record_request_rejection = lambda *args, **kwargs: None
    captured = {}
    handler._send = lambda status, body, *args, **kwargs: captured.update(status=status,payload=json.loads(body))
    handler._do_POST()
    cases.append({'data':base64.b64encode(data).decode(), 'length':handler.headers['Content-Length'], 'encoding':encoding, 'expected':captured})
json.dump(cases, sys.stdout)
"#;
    let output = Command::new(python)
        .args(["-c", script])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("Python body oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("body fixtures");
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = catalog_server(&upstream);
    for case in cases {
        let data = base64::engine::general_purpose::STANDARD
            .decode(case["data"].as_str().expect("base64"))
            .expect("body");
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        write!(stream,"POST /api/catalog/refresh HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nContent-Encoding: {}\r\n{}\r\nConnection: close\r\n\r\n",server.local_addr(),case["length"].as_str().expect("length"),case["encoding"].as_str().expect("encoding"),session_cookie_header(&server)).expect("request headers");
        stream.write_all(&data).expect("request body");
        stream.shutdown(Shutdown::Write).expect("finish request");
        let mut wire = String::new();
        stream.read_to_string(&mut wire).expect("response");
        let status: u16 = wire
            .split_whitespace()
            .nth(1)
            .expect("status")
            .parse()
            .expect("status number");
        assert_eq!(
            json!({"status":status,"payload":parsed_body(&wire)}),
            case["expected"]
        );
        assert_eq!(status, 413);
    }
    assert!(upstream.requests.try_recv().is_err());
    server.shutdown().expect("shutdown");
}

#[test]
fn discovery_preview_selection_and_model_endpoints_persist_across_restart() {
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let original = std::fs::read(&config_path).expect("config bytes");
    let cookie = session_cookie_header(&server);
    let preview = post(
        &server,
        "/api/providers/discover",
        br#"{"provider":"demo"}"#,
        &[&cookie],
    );
    assert!(preview.starts_with("HTTP/1.1 200"), "{preview}");
    let preview = parsed_body(&preview);
    assert_eq!(preview["available"], 2);
    assert_eq!(preview["models"][0]["upstream_id"], "new");
    assert_eq!(preview["added"], 0);
    assert_eq!(std::fs::read(&config_path).expect("config"), original);
    upstream.observed();

    let selected = post(
        &server,
        "/api/providers/discover",
        br#"{"provider":"demo","selected":["new"]}"#,
        &[&cookie],
    );
    assert!(selected.starts_with("HTTP/1.1 200"), "{selected}");
    let selected = parsed_body(&selected);
    assert_eq!(selected["added"], 1);
    assert_eq!(selected["hidden"], 1);
    assert_eq!(selected["model_count"], 2);
    upstream.observed();
    let generated = root
        .join("codex")
        .join("easy-multi-provider")
        .join("catalog.json");
    assert_eq!(selected["catalog_path"], json!(generated));
    let catalog: Value =
        serde_json::from_slice(&std::fs::read(&generated).expect("catalog file")).expect("catalog");
    assert!(
        catalog["models"]
            .as_array()
            .expect("models")
            .iter()
            .any(|model| model["slug"] == "demo/new")
    );
    assert!(
        !String::from_utf8(std::fs::read(&config_path).expect("config"))
            .expect("UTF8")
            .contains("synthetic-test-key")
    );
    let saved = load_configuration(Some(&config_path)).expect("saved config");
    assert_eq!(saved["models"][0]["enabled"], false);
    assert_eq!(
        provider_api_key(
            &saved["providers"][0],
            &server.state.backend.configuration.vault
        ),
        "synthetic-test-key"
    );

    let rich = request(&server, "/v1/models?client_version=0.155.0", &[]);
    assert!(rich.starts_with("HTTP/1.1 200"), "{rich}");
    assert!(rich.contains("ETag: \"emp-"), "{rich}");
    assert_eq!(parsed_body(&rich), catalog);
    assert!(
        generated
            .parent()
            .expect("parent")
            .join("native-catalog.json")
            .exists()
    );
    let list = parsed_body(&request(&server, "/v1/models", &[]));
    assert_eq!(list["object"], "list");
    assert_eq!(list["data"].as_array().map(Vec::len), Some(2));
    let model = parsed_body(&request(&server, "/v1/models/demo%2Fnew", &[]));
    assert_eq!(model, json!({"id":"demo/new","object":"model","created":0}));
    let unknown = request(&server, "/v1/models/demo%2Fold", &[]);
    assert!(unknown.starts_with("HTTP/1.1 404"));
    let refresh = post(&server, "/api/catalog/refresh", b"{}", &[&cookie]);
    assert_eq!(parsed_body(&refresh)["model_count"], 2);

    server.shutdown().expect("shutdown server");
    let restarted = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "codex",
        root.join("codex/auth.json"),
    )
    .expect("restart");
    assert_eq!(
        parsed_body(&request(
            &restarted,
            "/v1/models?client_version=0.155.0",
            &[]
        )),
        catalog
    );
    assert!(
        post(
            &restarted,
            "/api/providers/discover",
            br#"{"provider":"demo"}"#,
            &[&cookie]
        )
        .starts_with("HTTP/1.1 200")
    );
    upstream.observed();
    restarted.shutdown().expect("shutdown restarted server");
}

#[test]
fn discovery_authentication_precedes_body_and_invalid_selection_does_not_write() {
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = catalog_server(&upstream);
    let cookie = session_cookie_header(&server);
    let unauthenticated = post(&server, "/api/providers/discover", b"not json", &[]);
    assert!(unauthenticated.starts_with("HTTP/1.1 401"));
    let cross_origin = post(
        &server,
        "/api/providers/discover",
        b"not json",
        &[&cookie, "Origin: https://example.invalid"],
    );
    assert!(cross_origin.starts_with("HTTP/1.1 403"));
    let missing = post(&server, "/api/providers/discover", b"{}", &[&cookie]);
    assert!(missing.starts_with("HTTP/1.1 400"));
    assert_eq!(
        parsed_body(&missing)["error"]["message"],
        "provider is required"
    );
    assert!(upstream.requests.try_recv().is_err());
    let before = std::fs::read(&server.state.backend.configuration.config_path).expect("config");
    for selected in [json!(["not-advertised"]), json!(false), json!([1])] {
        let body =
            serde_json::to_vec(&json!({"provider":"demo","selected":selected})).expect("JSON");
        let invalid = post(&server, "/api/providers/discover", &body, &[&cookie]);
        assert!(invalid.starts_with("HTTP/1.1 400"), "{invalid}");
        upstream.observed();
        assert_eq!(
            std::fs::read(&server.state.backend.configuration.config_path).expect("config"),
            before
        );
    }
    server.shutdown().expect("shutdown server");
}

#[test]
fn discovery_upstream_failure_keeps_status_and_never_leaks_body() {
    let upstream = CatalogUpstream::start(503);
    let (_directory, server) = catalog_server(&upstream);
    let result = post(
        &server,
        "/api/providers/discover",
        br#"{"provider":"demo"}"#,
        &[&session_cookie_header(&server)],
    );
    assert!(result.starts_with("HTTP/1.1 503"), "{result}");
    assert!(!result.contains("private upstream diagnostic"));
    assert!(!result.contains("synthetic-test-key"));
    upstream.observed();
    server.shutdown().expect("shutdown server");
}

#[cfg(unix)]
#[test]
fn selection_rolls_back_config_and_keys_if_catalog_destination_is_unsafe() {
    use std::os::unix::fs::symlink;
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let catalog_dir = root.join("codex/easy-multi-provider");
    std::fs::create_dir_all(&catalog_dir).expect("catalog dir");
    let protected = root.join("protected.txt");
    std::fs::write(&protected, b"unchanged").expect("protected file");
    symlink(&protected, catalog_dir.join("catalog.json")).expect("unsafe destination");
    let before = std::fs::read(&server.state.backend.configuration.config_path).expect("config");
    let result = post(
        &server,
        "/api/providers/discover",
        br#"{"provider":"demo","selected":["new"]}"#,
        &[&session_cookie_header(&server)],
    );
    assert!(result.starts_with("HTTP/1.1 500"), "{result}");
    upstream.observed();
    assert_eq!(
        std::fs::read(&server.state.backend.configuration.config_path).expect("config"),
        before
    );
    assert_eq!(
        std::fs::read(&protected).expect("protected file"),
        b"unchanged"
    );
    let config = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .expect("config lock");
    assert_eq!(config["models"].as_array().map(Vec::len), Some(1));
    assert_eq!(config["models"][0]["enabled"], true);
    drop(config);
    server.shutdown().expect("shutdown server");
}
