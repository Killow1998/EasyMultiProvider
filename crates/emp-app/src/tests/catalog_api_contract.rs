use super::*;
use emp_transport::{HttpClient, HttpClientConfig, HttpClientPolicy, ProxyPolicy, TimeoutPolicy};

pub(super) struct CatalogUpstream {
    address: SocketAddr,
    requests: mpsc::Receiver<(String, BTreeMap<String, String>, Value)>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl CatalogUpstream {
    pub(super) fn start(status: u16) -> Self {
        Self::start_with_payload(
            status,
            json!({"data":[
                {"id":"new", "name":"New model", "context_length":128000, "supported_parameters":["tools","reasoning_effort"], "reasoning_levels":["low","high"]},
                {"id":"old", "context_length":64000}
            ]}),
        )
    }

    fn start_with_payload(status: u16, payload: Value) -> Self {
        Self::start_with_response(
            status,
            serde_json::to_vec(&payload).expect("payload JSON"),
            "application/json",
            None,
            None,
        )
    }

    fn start_with_redirect(payload: Value, from: &str, to: &str) -> Self {
        Self::start_with_response(
            200,
            serde_json::to_vec(&payload).expect("payload JSON"),
            "application/json",
            Some((from.to_owned(), to.to_owned())),
            None,
        )
    }

    fn start_with_raw_response(status: u16, payload: Vec<u8>) -> Self {
        Self::start_with_response(status, payload, "application/json", None, None)
    }

    fn start_with_delay(payload: Value, delay: Duration) -> Self {
        Self::start_with_response(
            200,
            serde_json::to_vec(&payload).expect("payload JSON"),
            "application/json",
            None,
            Some(delay),
        )
    }

    fn start_with_response(
        status: u16,
        payload: Vec<u8>,
        content_type: &'static str,
        redirect: Option<(String, String)>,
        delay: Option<Duration>,
    ) -> Self {
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
                        // Exercise inherited nonblocking mode on Linux too, then use the
                        // blocking reads expected by this synchronous fake upstream.
                        stream
                            .set_nonblocking(true)
                            .expect("nonblocking accepted discovery stream");
                        stream
                            .set_nonblocking(false)
                            .expect("blocking accepted discovery stream");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("discovery request timeout");
                        let Some(raw) = read_request_head(&mut stream) else {
                            continue;
                        };
                        let (path, headers, body) =
                            receive_upstream_request_from_head(&mut stream, raw);
                        sender
                            .send((path.clone(), headers, body))
                            .expect("record request");
                        if let Some((from, to)) = &redirect
                            && &path == from
                        {
                            write!(stream,"HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").expect("redirect head");
                            continue;
                        }
                        write!(stream,"HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n",status_text(status),payload.len()).expect("head");
                        if let Some(delay) = delay {
                            thread::sleep(delay);
                        }
                        let _ = stream.write_all(&payload);
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

    fn observed_metadata(&self, expected_paths: &[&str]) -> Vec<BTreeMap<String, String>> {
        expected_paths
            .iter()
            .map(|expected_path| {
                let (path, headers, body) = self
                    .requests
                    .recv_timeout(Duration::from_secs(5))
                    .expect("metadata request");
                assert_eq!(&path, expected_path);
                assert_eq!(headers["x-goog-api-key"], "synthetic-test-key");
                assert!(!headers.contains_key("authorization"));
                assert_eq!(body, Value::Null);
                headers
            })
            .collect()
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

fn metadata_server(upstream: &CatalogUpstream) -> (TempDir, ServerHandle) {
    metadata_server_with_policy(upstream, HttpClientPolicy::default())
}

fn metadata_server_with_policy(
    upstream: &CatalogUpstream,
    policy: HttpClientPolicy,
) -> (TempDir, ServerHandle) {
    let mut transport_config = HttpClientConfig::default();
    transport_config
        .add_dns_override("generativelanguage.googleapis.com", upstream.address)
        .expect("Gemini DNS override");
    let client = HttpClient::with_config(policy, transport_config).expect("metadata HTTP client");
    metadata_server_with_http_client(upstream, client)
}

fn metadata_server_with_http_client(
    upstream: &CatalogUpstream,
    client: HttpClient,
) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("metadata temp dir");
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let native_path = root.join("codex").join("models_cache.json");
    std::fs::create_dir_all(native_path.parent().expect("parent")).expect("native dir");
    std::fs::write(&native_path, br#"{"models":[]}"#).expect("native catalog");
    let base_url = format!("http://{}/v1beta/openai", upstream.address);
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "native_catalog_path":native_path,
            "providers":[{
                "id":"gemini", "base_url":base_url, "protocol":"chat_completions",
                "auth_mode":"api_key", "api_key":"synthetic-test-key"
            }],
            "models":[]
        }))
        .expect("metadata config JSON"),
    )
    .expect("write metadata config");
    let server = ServerHandle::start_with_config_options_and_http_client(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "codex",
        root.join("codex").join("auth.json"),
        client,
    )
    .expect("metadata server");
    // The persisted config validator correctly rejects insecure non-loopback
    // URLs. After startup, point this test-only in-memory provider at the
    // Gemini host so model_metadata's host gate and DNS override both run.
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .expect("metadata config lock")
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .and_then(|providers| providers.first_mut())
        .expect("metadata provider")
        .as_object_mut()
        .expect("metadata provider object")["base_url"] = json!(format!(
        "http://generativelanguage.googleapis.com:{}/v1beta/openai",
        upstream.address.port()
    ));
    (directory, server)
}

pub(super) fn parsed_body(wire: &str) -> Value {
    serde_json::from_str(wire.split_once("\r\n\r\n").expect("response head").1).expect("JSON")
}

#[test]
fn catalog_http_serves_models_discovery_metadata_and_account_routes() {
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = catalog_server(&upstream);
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .expect("config lock")
        .as_object_mut()
        .expect("config object")["providers"]
        .as_array_mut()
        .expect("providers")
        .push(json!({"id":"disabled","base_url":"https://example.invalid/v1","enabled":false}));
    let cookie = session_header(&server);
    let cases = json!([
        {"path":"/v1/models"},
        {"path":"/v1/models?client_version="},
        {"path":"/v1/models?client_version=0.156.1"},
        {"path":"/v1/models/demo%2Fold"},
        {"path":"/v1/models/missing"},
        {"path":"/api/providers/discover", "body":{"provider":"demo"}},
        {"path":"/api/providers/discover", "body":{}},
        {"path":"/api/providers/discover", "body":{"provider":"absent"}},
        {"path":"/api/providers/discover", "body":{"provider":"demo", "selected":false}},
        {"path":"/api/providers/discover", "body":{"provider":"demo", "selected":["missing"]}},
        {"path":"/api/models/metadata", "body":{}},
        {"path":"/api/models/metadata", "body":{"provider":42,"model":"model"}},
        {"path":"/api/models/metadata", "body":{"provider":"demo"}},
        {"path":"/api/models/metadata", "body":{"provider":"absent","model":"model"}},
        {"path":"/api/models/metadata", "body":{"provider":"disabled","model":"model"}},
        {"path":"/api/models/metadata", "body":{"provider":"demo","model":"demo/model"}},
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
    // The Rust handler is the contract: every case must return a JSON object
    // with a stable status, and the discover call records what it fetched.
    // Case 2 is the rich client list: it carries the generated catalog ETag.
    // Case 3 resolves a known model id; case 4 is an unknown id (404).
    assert_eq!(actual[2]["status"], 200, "rich client list resolves");
    assert!(
        actual[2]["etag"].is_string(),
        "rich client list carries an ETag"
    );
    assert!(
        actual[2]["payload"]["models"].is_array(),
        "the rich client list returns the generated catalog document"
    );
    assert_eq!(actual[3]["status"], 200, "a valid model id resolves");
    assert_eq!(actual[4]["status"], 404, "an unknown model id is not found");
    assert_eq!(actual[5]["status"], 200, "provider discovery succeeds");
    assert!(actual[5]["payload"]["models"].is_array());
    assert_eq!(
        actual[6]["status"], 400,
        "an empty discover body is rejected"
    );
    assert!(
        actual[6]["payload"]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("provider is required")),
        "{}",
        actual[6]["payload"]
    );
    // The demo provider is not on the auto-discovery host, so the metadata
    // operation rejects it with a stable request error instead of guessing.
    assert_eq!(actual[15]["status"], 400, "non-gemini metadata is rejected");
    // A missing or empty account id is a request error, never a catalog.
    assert_eq!(
        actual[20]["status"], 400,
        "account models need an account id"
    );
    assert_eq!(actual[21]["status"], 400, "empty account id is not a route");
    server.shutdown().expect("shutdown server");
}

#[test]
fn model_metadata_follows_redirects_and_projects_gemini_limits() {
    let upstream_payload = json!({
        "inputTokenLimit":1_048_576,
        "outputTokenLimit":65_536,
        "thinking":true,
        "reasoning_levels":["low","high"],
        "supports_reasoning_summaries":true
    });
    let initial_path = "/v1beta/models/alpha%20beta%2Fpreview";
    let redirect_path = "/v1beta/models/redirect-target";
    let upstream =
        CatalogUpstream::start_with_redirect(upstream_payload.clone(), initial_path, redirect_path);
    let (directory, server) = metadata_server(&upstream);
    let body = json!({"provider":"gemini","model":"gemini/alpha beta/preview"});
    let wire = post(
        &server,
        "/api/models/metadata",
        &serde_json::to_vec(&body).expect("metadata request JSON"),
        &[&session_header(&server)],
    );
    let status: u16 = wire
        .split_whitespace()
        .nth(1)
        .expect("response status")
        .parse()
        .expect("numeric status");
    assert_eq!(status, 200);
    // The Rust handler is the contract: the payload projects the Gemini
    // limits, reasoning flags, and levels from the upstream document.
    assert_eq!(
        parsed_body(&wire),
        json!({
            "model":"alpha beta/preview",
            "context_window":1_048_576,
            "input_token_limit":1_048_576,
            "output_token_limit":65_536,
            "supports_reasoning":true,
            "supports_reasoning_summaries":true,
            "reasoning_levels":["low","high"],
        })
    );
    // Percent-encoded model segments survive the route, across the redirect,
    // and the Gemini API key travels on every hop without persisting.
    let headers = upstream.observed_metadata(&[initial_path, redirect_path]);
    assert_eq!(headers[0]["x-goog-api-key"], "synthetic-test-key");
    assert_eq!(headers[1]["x-goog-api-key"], "synthetic-test-key");
    assert!(
        !std::fs::read_to_string(canonical_root(&directory).join("config.json"))
            .expect("saved configuration")
            .contains("synthetic-test-key"),
        "the metadata operation must not persist provider credentials"
    );
    server.shutdown().expect("shutdown metadata server");
}

#[test]
fn model_metadata_maps_upstream_errors_to_stable_api_errors() {
    let body = json!({"provider":"gemini","model":"gemini/error-model"});
    let cases = [
        ("upstream 429", 429, "rate_limit"),
        ("malformed JSON", 200, "upstream_5xx"),
        ("missing token limits", 200, "upstream_5xx"),
        ("oversized body", 200, "upstream_5xx"),
    ];
    for (label, upstream_status, expected_error) in cases {
        let payload = match label {
            "upstream 429" => br#"{"error":{"message":"synthetic quota detail"}}"#.to_vec(),
            "malformed JSON" => b"not-json".to_vec(),
            "missing token limits" => br#"{"inputTokenLimit":1024}"#.to_vec(),
            _ => vec![b'x'; 4 * 1024 * 1024 + 1],
        };
        let upstream = CatalogUpstream::start_with_raw_response(upstream_status, payload);
        let (_directory, server) = metadata_server(&upstream);
        let wire = post(
            &server,
            "/api/models/metadata",
            &serde_json::to_vec(&body).expect("metadata request JSON"),
            &[&session_header(&server)],
        );
        let status: u16 = wire
            .split_whitespace()
            .nth(1)
            .expect("response status")
            .parse()
            .expect("numeric status");
        let payload = parsed_body(&wire);
        assert_eq!(payload["error"]["type"], expected_error, "{label}");
        if upstream_status == 429 {
            assert_eq!(status, 429, "{label}");
            assert_eq!(payload["error"]["code"], "rate_limit_exceeded", "{label}");
            assert!(
                payload["error"]["message"]
                    .as_str()
                    .expect("message")
                    .contains("429"),
                "{label}: the upstream status is surfaced"
            );
        } else {
            assert_eq!(status, 502, "{label}");
        }
        server.shutdown().expect("shutdown metadata server");
    }
}

#[test]
fn model_metadata_surfaces_a_timeout_as_a_502_error() {
    let body = json!({"provider":"gemini","model":"gemini/slow-model"});
    let payload = json!({"inputTokenLimit":1024,"outputTokenLimit":128});
    let upstream = CatalogUpstream::start_with_delay(payload, Duration::from_millis(300));
    let policy = HttpClientPolicy::new(
        ProxyPolicy::default(),
        TimeoutPolicy {
            non_stream_wall_clock: Duration::from_millis(40),
            ..TimeoutPolicy::default()
        },
    );
    let (_directory, server) = metadata_server_with_policy(&upstream, policy);
    let wire = post(
        &server,
        "/api/models/metadata",
        &serde_json::to_vec(&body).expect("metadata request JSON"),
        &[&session_header(&server)],
    );
    let status: u16 = wire
        .split_whitespace()
        .nth(1)
        .expect("response status")
        .parse()
        .expect("numeric status");
    assert_eq!(status, 502);
    let payload = parsed_body(&wire);
    // The wall-clock timeout maps to the stable upstream-5xx transport class;
    // the message still names the timeout cause.
    assert_eq!(payload["error"]["type"], "upstream_5xx");
    assert!(
        payload["error"]["message"]
            .as_str()
            .expect("message")
            .contains("timed out"),
        "the timeout cause is surfaced to the user"
    );
    let _headers = upstream.observed_metadata(&["/v1beta/models/slow-model"]);
    server.shutdown().expect("shutdown metadata server");
}

#[test]
fn model_metadata_http_authentication_and_body_errors_are_enforced() {
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = metadata_server(&upstream);
    let unauthenticated = post(&server, "/api/models/metadata", b"not json", &[]);
    assert!(
        unauthenticated.starts_with("HTTP/1.1 401"),
        "{unauthenticated}"
    );
    let cookie = session_header(&server);
    let cross_origin = post(
        &server,
        "/api/models/metadata",
        b"not json",
        &[&cookie, "Origin: https://example.invalid"],
    );
    assert!(cross_origin.starts_with("HTTP/1.1 403"), "{cross_origin}");
    let non_object = post(&server, "/api/models/metadata", b"[]", &[&cookie]);
    assert!(non_object.starts_with("HTTP/1.1 400"), "{non_object}");
    assert_eq!(
        parsed_body(&non_object)["error"]["message"],
        "request body must be a JSON object"
    );
    let bad_content_type = post(
        &server,
        "/api/models/metadata",
        b"{}",
        &[&cookie, "Content-Type: text/plain"],
    );
    assert!(
        bad_content_type.starts_with("HTTP/1.1 400"),
        "{bad_content_type}"
    );
    assert_eq!(
        parsed_body(&bad_content_type)["error"]["message"],
        "Content-Type must be application/json"
    );
    assert!(upstream.requests.try_recv().is_err());
    server.shutdown().expect("shutdown metadata server");
}

#[test]
fn management_body_limits_reject_oversized_plain_and_gzip_bodies() {
    // The management JSON limit is 5 MiB, applied after Content-Length checks
    // and again after decompressing a gzip Content-Encoding body. Both
    // bodies exceed the ceiling on the wire, so the wire limit rejects them
    // before parsing.
    let limit = 5 * 1024 * 1024;
    let payload = format!(r#"{{"padding":"{}"}}"#, "x".repeat(limit + 32));
    let plain = payload.clone().into_bytes();
    let gzip = {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&plain).expect("gzip body");
        encoder.finish().expect("gzip finish")
    };
    let cases = vec![
        ("plain", plain, (limit + 32).to_string(), "identity"),
        ("gzip", gzip, String::new(), "gzip"),
    ];
    let upstream = CatalogUpstream::start(200);
    let (_directory, server) = catalog_server(&upstream);
    for (label, data, length, encoding) in cases {
        let mut stream = TcpStream::connect(server.local_addr()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let length = if length.is_empty() {
            data.len().to_string()
        } else {
            length
        };
        write!(stream,"POST /api/catalog/refresh HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nContent-Encoding: {}\r\n{}\r\nConnection: close\r\n\r\n",server.local_addr(),length,encoding,session_header(&server)).expect("request headers");
        // A declared length over the limit is rejected from the headers
        // alone. Sending that body anyway races the server's close, and some
        // platforms turn the race into a reset that drops the 413 response.
        if data.len() <= limit {
            stream.write_all(&data).expect("request body");
        }
        let _ = stream.shutdown(Shutdown::Write);
        let mut wire = String::new();
        stream.read_to_string(&mut wire).expect("response");
        let status: u16 = wire
            .split_whitespace()
            .nth(1)
            .expect("status")
            .parse()
            .expect("status number");
        assert_eq!(status, 413, "{label} body is rejected");
        assert!(
            !parsed_body(&wire).to_string().contains("padding"),
            "{label}: the oversized body is not echoed back"
        );
    }
    assert!(upstream.requests.try_recv().is_err());
    server.shutdown().expect("shutdown");
}

#[test]
fn discovery_preview_selection_and_model_endpoints_persist_across_restart() {
    let upstream = CatalogUpstream::start(200);
    drop(TcpStream::connect(upstream.address).expect("empty discovery connection"));
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let config_path = root.join("config.json");
    let original = std::fs::read(&config_path).expect("config bytes");
    let cookie = session_header(&server);
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

    let rich = request(&server, "/v1/models?client_version=0.156.1", &[]);
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
            "/v1/models?client_version=0.156.1",
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
    let cookie = session_header(&server);
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
        &[&session_header(&server)],
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
        &[&session_header(&server)],
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
        .test_config()
        .lock()
        .expect("config lock");
    assert_eq!(config["models"].as_array().map(Vec::len), Some(1));
    assert_eq!(config["models"][0]["enabled"], true);
    drop(config);
    server.shutdown().expect("shutdown server");
}
