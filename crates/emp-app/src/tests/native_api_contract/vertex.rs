//! Codex and EMP against a local TLS Vertex fixture, with no cloud credentials.
use super::installed_codex::Codex;
use super::*;
use emp_test_support::OwnedChild;
use emp_transport::{HttpClient, HttpClientConfig, HttpClientPolicy};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, date_time_ymd,
};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};
use std::process::{Command, Stdio};

pub(crate) struct VertexFixture {
    host: String,
    address: SocketAddr,
    root: Vec<u8>,
    requests: mpsc::Receiver<Value>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub(crate) fn fixture_tls(host: &str) -> (Arc<ServerConfig>, Vec<u8>) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.not_before = date_time_ymd(2025, 1, 1);
    ca.not_after = date_time_ymd(2030, 1, 1);
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
    ];
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca, ca_key);
    let mut leaf = CertificateParams::new(vec![host.to_owned()]).unwrap();
    leaf.not_before = date_time_ymd(2025, 1, 1);
    leaf.not_after = date_time_ymd(2030, 1, 1);
    leaf.distinguished_name = DistinguishedName::new();
    leaf.distinguished_name.push(DnType::CommonName, host);
    leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    leaf.use_authority_key_identifier_extension = true;
    let leaf_key = KeyPair::generate().unwrap();
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
    let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
    let config = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf_cert.der().clone(), ca_cert.der().clone()],
                private_key,
            )
            .unwrap(),
    );
    (config, ca_cert.der().to_vec())
}

impl VertexFixture {
    fn start() -> Self {
        Self::start_for("aiplatform.googleapis.com")
    }

    pub(crate) fn start_for(host: &str) -> Self {
        let (config, root) = fixture_tls(host);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                let Ok((stream, _)) = accept_blocking(&listener) else {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                    )]
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                let connection = ServerConnection::new(Arc::clone(&config)).unwrap();
                let mut input = BufReader::new(StreamOwned::new(
                    connection,
                    Deadline::after(test_timeout(Duration::from_secs(10))).io(stream),
                ));
                let mut first = String::new();
                input.read_line(&mut first).unwrap();
                let mut headers = BTreeMap::new();
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    let (key, value) = line.split_once(':').unwrap();
                    headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
                }
                let mut bytes = vec![
                    0;
                    headers
                        .get("content-length")
                        .map(|v| v.parse::<usize>().unwrap())
                        .unwrap_or(0)
                ];
                input.read_exact(&mut bytes).unwrap();
                let body: Value = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                let mut status = "200 OK";
                let result = if first.contains(":countTokens") {
                    if ["gemini-2.5-flash", "gemini-future-fixture"]
                        .iter()
                        .any(|model| first.contains(&format!("/{model}:countTokens")))
                    {
                        json!({"totalTokens":1})
                    } else {
                        status = "404 Not Found";
                        json!({"error":{"status":"NOT_FOUND"}})
                    }
                } else if first.starts_with("POST /v1/images/") {
                    json!({"data":[{"b64_json":"ZmFrZQ=="}],"usage":{"input_tokens":10,"output_tokens":400,
                        "input_tokens_details":{"text_tokens":4,"image_tokens":6}}})
                } else if first.contains("/v1beta1/publishers/google/models") {
                    if first.contains("pageToken=") {
                        json!({"publisherModels":[{"name":"publishers/google/models/gemini-3-flash", "versionId":"gemini-3-flash-preview"}]})
                    } else {
                        json!({"publisherModels":[{"name":"publishers/google/models/gemini-2.5-flash"},
                            {"name":"publishers/google/models/gemini-4-hydrogen"},
                            {"name":"publishers/google/models/gemini-future-fixture"},
                            {"name":"publishers/google/models/gemini-embedding-001"},
                            {"name":"publishers/google/models/gemini-2.5-flash-preview-tts"}],"nextPageToken":"next"})
                    }
                } else if first.contains("/v1beta1/publishers/anthropic/models") {
                    json!({"publisherModels":[{"name":"publishers/anthropic/models/claude-sonnet-4","versionId":"20250514"}]})
                } else if first.contains("/endpoints/openapi/chat/completions") {
                    json!({"id":"chatcmpl-fixture","object":"chat.completion","created":1,"model":"google/gemini-3-flash-preview",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"ADC Gemini completed."},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":4,"completion_tokens":2,"total_tokens":6}})
                } else if first.contains(":rawPredict") {
                    json!({"id":"msg-fixture","type":"message","role":"assistant","model":"claude-sonnet-4-6",
                        "content":[{"type":"text","text":"ADC Claude completed."}],"stop_reason":"end_turn",
                        "usage":{"input_tokens":4,"output_tokens":2}})
                } else if body["contents"].to_string().contains("THOUGHT_ONLY") {
                    json!({"modelVersion":"gemini-3-flash-preview-actual",
                        "candidates":[{"content":{"parts":[{"thought":true,"text":"fixture reasoning"}]},"finishReason":"MAX_TOKENS"}],
                        "usageMetadata":{"promptTokenCount":4,"thoughtsTokenCount":7,"totalTokenCount":11}})
                } else {
                    let has_result = body["contents"].as_array().unwrap().iter().any(|content| {
                        content["parts"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|part| part.get("functionResponse").is_some())
                    });
                    let names = body
                        .pointer("/tools/0/functionDeclarations")
                        .and_then(Value::as_array)
                        .unwrap();
                    let name = names
                        .iter()
                        .filter_map(|item| item["name"].as_str())
                        .find(|name| {
                            name.ends_with("exec_command") || name.ends_with("shell_command")
                        })
                        .unwrap_or("read");
                    let args = if name.ends_with("shell_command") {
                        json!({"command":"pwd"})
                    } else if name.ends_with("exec_command") {
                        json!({"cmd":"pwd"})
                    } else {
                        json!({})
                    };
                    let part = if has_result {
                        json!({"text":"Vertex fixture completed."})
                    } else if name == "read" {
                        json!({"thoughtSignature":"fixture-signature", "functionCall":{"name":name}})
                    } else {
                        json!({"thoughtSignature":"fixture-signature", "functionCall":{"name":name,"args":args}})
                    };
                    json!({"modelVersion":"gemini-3-flash-preview-actual","candidates":[{"content":{"role":"model","parts":[part]},"finishReason":"STOP"}],
                        "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}})
                };
                sender.send(json!({"path":first.split_whitespace().nth(1).unwrap(),"headers":headers,"body":body})).unwrap();
                let stream = first.contains("streamGenerateContent");
                let payload = if stream {
                    format!("data: {result}\n\n")
                } else {
                    result.to_string()
                };
                let mut output = input.into_inner();
                if first.starts_with("POST /v1/images/") && body["prompt"] == "truncate-response" {
                    write!(output, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{").unwrap();
                    output.flush().unwrap();
                    continue;
                }

                write!(output, "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    if stream { "text/event-stream" } else { "application/json" }, payload.len(), payload).unwrap();
                output.flush().unwrap();
            }
        });
        Self {
            host: host.to_owned(),
            address,
            root,
            requests,
            stop,
            worker: Some(worker),
        }
    }

    pub(crate) fn client(&self) -> HttpClient {
        let mut config = HttpClientConfig::default();
        config.add_root_certificate_der(&self.root).unwrap();
        config.use_only_configured_root_certificates().unwrap();
        config.add_dns_override(&self.host, self.address).unwrap();
        HttpClient::with_config(HttpClientPolicy::default(), config).unwrap()
    }

    pub(super) fn received(&self) -> Value {
        self.requests
            .recv_timeout(Duration::from_secs(15))
            .expect("TLS fixture request")
    }
}

impl Drop for VertexFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("Vertex fixture worker panicked");
        }
    }
}

fn server(root: &Path, upstream: &VertexFixture) -> ServerHandle {
    ServerHandle::start_with_config_options_and_http_client(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &root.join("config.json"),
        "codex",
        root.join("codex").join("auth.json"),
        upstream.client(),
    )
    .unwrap()
}

fn setup(root: &Path) {
    std::fs::create_dir_all(root.join("codex")).unwrap();
    std::fs::write(root.join("codex/native.json"), br#"{"models":[]}"#).unwrap();
    let config = json!({"native_catalog_path":root.join("codex/native.json"),"providers":[{"id":"vertex","name":"Vertex","protocol":"auto","auth_mode":"vertex_api_key",
        "vertex":{"project":"","location":"global"},"api_key":"fixture-only"}],
        "models":[{"id":"vertex/gemini-3-flash-preview","provider":"vertex","upstream_id":"gemini-3-flash-preview","enabled":true}]});
    let catalog = emp_codex::merged_catalog::build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    std::fs::write(
        root.join("codex/models.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.join("config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
}

fn assert_signature(body: &Value) {
    let parts = body["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["parts"].as_array().unwrap());
    assert!(
        parts
            .into_iter()
            .any(|part| part["thoughtSignature"] == "fixture-signature")
    );
}

#[test]
fn vertex_key_discovers_litellm_models_and_caches_express_count_tokens() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let config_path = root.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    let mut regional = config["providers"][0].clone();
    regional["id"] = json!("vertex-project");
    regional["name"] = json!("Vertex project");
    regional["vertex"]["project"] = json!("fixture-project");
    config["providers"].as_array_mut().unwrap().push(regional);
    std::fs::write(&config_path, config.to_string()).unwrap();

    let now = crate::util::system_now();
    let prices =
        emp_state::usage::pricing::PriceCatalog::new(root.join("state/api_prices.json"), now);
    prices
        .save_litellm_snapshot(
            &json!({
                "vertex_ai/gemini-future-fixture":{"litellm_provider":"vertex_ai","mode":"chat",
                    "input_cost_per_token":0.001,"output_cost_per_token":0.002,
                    "max_input_tokens":123456,"max_output_tokens":4096,
                    "supports_vision":true,"supports_reasoning":true},
                "gemini-2.5-flash":{"litellm_provider":"vertex_ai-language-models","mode":"chat",
                    "max_input_tokens":123,"max_output_tokens":456,"supports_vision":false},
                "gemini-unsupported-fixture":{"litellm_provider":"vertex_ai-language-models","mode":"chat"},
                "gemini-image-fixture":{"litellm_provider":"vertex_ai","mode":"image_generation"},
                "gemini/gemini-developer-fixture":{"litellm_provider":"gemini","mode":"chat"},
            }),
            now - 1.0,
        )
        .unwrap();
    let upstream = VertexFixture::start();
    let server = server(&root, &upstream);
    let token = format!("X-EMP-Session: {}", server.session_token());
    let origin = format!("Origin: http://{}", server.local_addr());
    let discover = |body: Value| {
        let wire = post(
            &server,
            "/api/providers/discover",
            body.to_string().as_bytes(),
            &[&token, &origin],
        );
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        serde_json::from_str::<Value>(wire.split_once("\r\n\r\n").unwrap().1).unwrap()
    };
    let reference = discover(json!({"provider":"vertex-project"}));
    assert!(upstream.requests.try_recv().is_err());
    let reference = reference["models"].as_array().unwrap();
    assert!(
        reference
            .iter()
            .any(|model| model["upstream_id"] == "gemini-unsupported-fixture")
    );
    assert!(!reference.iter().any(|model| matches!(
        model["upstream_id"].as_str(),
        Some("gemini-image-fixture" | "gemini-developer-fixture")
    )));
    let express = discover(json!({"provider":"vertex"}));
    let models = express["models"].as_array().unwrap();
    assert_eq!(models.len(), 2);
    let known = &models[0];
    assert_eq!(known["upstream_id"], "gemini-2.5-flash");
    assert_eq!(known["catalog_source"], "official");
    assert!(known["context_window"].as_u64().unwrap() > 123);
    assert_eq!(known["max_input_tokens"], 123);
    let future = &models[1];
    assert_eq!(future["upstream_id"], "gemini-future-fixture");
    assert_eq!(future["catalog_source"], "litellm");
    assert_eq!(future["availability"], "count_tokens_verified");
    assert_eq!(future["context_window"], 123456);
    assert_eq!(future["output_limit"], 4096);
    assert_eq!(future["supports_reasoning"], true);
    assert_eq!(future["input_modalities"], json!(["text", "image"]));
    let probes: Vec<_> = upstream.requests.try_iter().collect();
    assert!(probes.iter().any(|probe| probe["path"]
        == "/v1/publishers/google/models/gemini-unsupported-fixture:countTokens"));
    for probe in probes {
        assert_eq!(probe["headers"]["x-goog-api-key"], "fixture-only");
        assert!(probe["headers"]["authorization"].is_null());
        assert_eq!(probe["body"]["contents"][0]["parts"][0]["text"], "Hello");
    }
    assert_eq!(
        discover(json!({"provider":"vertex"}))["updated_at"],
        express["updated_at"]
    );
    assert_eq!(
        discover(json!({"provider":"vertex","cached":true,"selected":["gemini-future-fixture"]}))["added"],
        1
    );
    assert!(upstream.requests.try_recv().is_err());
    server.shutdown().unwrap();
}

#[test]
fn vertex_key_keeps_thought_only_usage_and_reported_model_in_call_records() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let upstream = VertexFixture::start();
    let server = server(&root, &upstream);
    let token = format!("X-EMP-Session: {}", server.session_token());
    for stream in [false, true] {
        let session = format!("thought-only-{stream}");
        let thread = format!("thread-id: {session}");
        let body =
            json!({"model":"vertex/gemini-3-flash-preview","input":"THOUGHT_ONLY","stream":stream})
                .to_string();
        let wire = if stream {
            post_stream(
                &server,
                "/v1/responses",
                body.as_bytes(),
                &[&token, &thread],
            )
        } else {
            post(
                &server,
                "/v1/responses",
                body.as_bytes(),
                &[&token, &thread],
            )
        };
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        upstream.received();
        let wire = crate::tests::request(&server, "/api/calls?start=0&end=9999999999", &[&token]);
        let report: Value = serde_json::from_str(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
        let record = report["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["session_id"] == session)
            .unwrap();
        assert_eq!(record["output_tokens"], 7, "{record}");
        assert_eq!(record["response_model"], "gemini-3-flash-preview-actual");
        assert_ne!(record["model_name_status"], "missing");
    }
    server.shutdown().unwrap();
}

#[test]
fn vertex_key_roundtrips_a_tool_signature_after_emp_restart() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let config_path = root.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["providers"][0]["vertex"]["project"] = json!("fixture-project");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let upstream = VertexFixture::start();
    let first = server(&root, &upstream);
    let input = json!([{"type":"message","role":"user","content":[
        {"type":"input_text","text":"Read"},
        {"type":"input_audio","audio_url":"data:audio/mpeg;base64,SUQz"}
    ]}]);
    let tools =
        json!([{"type":"function","name":"read","parameters":{"type":"object","properties":{}}}]);
    let body =
        json!({"model":"vertex/gemini-3-flash-preview","input":input,"tools":tools,"stream":false});
    let token = format!("X-EMP-Session: {}", first.session_token());
    let result = post(
        &first,
        "/v1/responses",
        body.to_string().as_bytes(),
        &[&token],
    );
    assert!(result.starts_with("HTTP/1.1 200"), "{result}");
    let response: Value = serde_json::from_str(result.split_once("\r\n\r\n").unwrap().1).unwrap();
    let call = response["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    assert_eq!(call["arguments"], "{}");
    let call_id = call["call_id"].as_str().unwrap().to_owned();
    let initial = upstream.received();
    assert_eq!(initial["headers"]["x-goog-api-key"], "fixture-only");
    assert_eq!(
        initial["body"]["contents"][0]["parts"][1],
        json!({"inlineData":{"mimeType":"audio/mpeg","data":"SUQz"}})
    );
    assert!(
        initial["path"]
            .as_str()
            .unwrap()
            .contains("/projects/fixture-project/locations/global/publishers/google/models/gemini-3-flash-preview:generateContent")
    );
    first.shutdown().unwrap();

    let second = server(&root, &upstream);
    let history = json!([input[0], {"type":"function_call","call_id":call_id,"name":"read","arguments":"{}"},
        {"type":"function_call_output","call_id":call_id,"output":"done"}]);
    let body = json!({"model":"vertex/gemini-3-flash-preview","input":history,"tools":tools,"stream":true});
    let token = format!("X-EMP-Session: {}", second.session_token());
    let result = post_stream(
        &second,
        "/v1/responses",
        body.to_string().as_bytes(),
        &[&token],
    );
    assert!(
        result.starts_with("HTTP/1.1 200") && result.contains("Vertex fixture completed."),
        "{result}"
    );
    let continued = upstream.received();
    assert_signature(&continued["body"]);
    assert_eq!(
        continued["body"]["contents"][0]["parts"][1],
        initial["body"]["contents"][0]["parts"][1]
    );
    second.shutdown().unwrap();
}

#[test]
#[ignore = "temporarily sets ADC environment and requires isolated loopback access; run alone"]
fn vertex_adc_routes_gemini_and_claude_with_refreshed_oauth() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let oauth = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let token_url = format!("http://{}/token", oauth.local_addr().unwrap());
    let token_worker = thread::spawn(move || {
        for number in 1..=2 {
            let (stream, _) = oauth.accept().unwrap();
            let mut input =
                BufReader::new(Deadline::after(test_timeout(Duration::from_secs(10))).io(stream));
            let mut first = String::new();
            input.read_line(&mut first).unwrap();
            assert!(first.starts_with("POST /token "));
            let mut length = 0;
            loop {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut ignored_body = vec![0; length];
            input.read_exact(&mut ignored_body).unwrap();
            let body = json!({"access_token":format!("fixture-oauth-token-{number}"),"token_type":"Bearer","expires_in":if number == 1 {1} else {3600}}).to_string();
            let mut output = input.into_inner();
            write!(output, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let credential_path = root.join("fake-adc.json");
    std::fs::write(
        &credential_path,
        json!({"type":"authorized_user","client_id":"fixture-id",
        "client_secret":"fixture-secret","refresh_token":"fixture-refresh","token_uri":token_url})
        .to_string(),
    )
    .unwrap();
    let previous = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
    // This ignored test runs alone, with a synthetic file and no other test threads.
    // SAFETY: the isolated runner guarantees no concurrent environment access; the token worker only serves loopback HTTP.
    unsafe {
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", &credential_path);
    }
    let config_path = root.join("config.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["providers"][0]["auth_mode"] = json!("vertex_adc");
    config["providers"][0]["api_key"] = json!("");
    config["providers"][0]["vertex"]["project"] = json!("fixture-project");
    config["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"vertex/claude-sonnet-4-6",
        "provider":"vertex","upstream_id":"claude-sonnet-4-6","enabled":true}));
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let upstream = VertexFixture::start();
    let server = server(&root, &upstream);
    let token = format!("X-EMP-Session: {}", server.session_token());
    for (index, (model, expected)) in [
        ("vertex/gemini-3-flash-preview", "ADC Gemini completed."),
        ("vertex/claude-sonnet-4-6", "ADC Claude completed."),
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            #[expect(
                clippy::disallowed_methods,
                reason = "等待首个令牌真实过期，验证后续请求会刷新并复用新令牌"
            )]
            thread::sleep(Duration::from_secs(2));
        }
        let body = json!({"model":model,"input":"Hello","stream":false});
        let result = post(
            &server,
            "/v1/responses",
            body.to_string().as_bytes(),
            &[&token],
        );
        assert!(
            result.starts_with("HTTP/1.1 200") && result.contains(expected),
            "{result}"
        );
        let sent = upstream.received();
        assert_eq!(
            sent["headers"]["authorization"],
            format!("Bearer fixture-oauth-token-{}", index + 1)
        );
        assert!(
            sent["path"]
                .as_str()
                .unwrap()
                .contains("projects/fixture-project/locations/global")
        );
        if model.contains("claude") {
            assert!(
                sent["path"]
                    .as_str()
                    .unwrap()
                    .contains("publishers/anthropic/models/claude-sonnet-4-6:rawPredict")
            );
            assert_eq!(sent["body"]["anthropic_version"], "vertex-2023-10-16");
            assert!(sent["body"].get("model").is_none());
        } else {
            assert_eq!(sent["body"]["model"], "google/gemini-3-flash-preview");
        }
    }
    let origin = format!("Origin: http://{}", server.local_addr());
    let result = post(
        &server,
        "/api/providers/discover",
        br#"{"provider":"vertex","cache_refresh":true}"#,
        &[&token, &origin],
    );
    assert!(result.starts_with("HTTP/1.1 200"), "{result}");
    for _ in 0..3 {
        let request = upstream.received();
        assert_eq!(
            request["headers"]["authorization"],
            "Bearer fixture-oauth-token-2"
        );
    }
    let result = request(&server, "/api/providers/vertex/models", &[&token]);
    assert!(
        result.contains("gemini-2.5-flash")
            && result.contains("gemini-4-hydrogen")
            && result.contains("gemini-future-fixture")
            && result.contains("gemini-3-flash-preview")
            && result.contains("claude-sonnet-4@20250514"),
        "{result}"
    );
    assert!(
        !result.contains("gemini-embedding-001")
            && !result.contains("gemini-2.5-flash-preview-tts")
    );
    server.shutdown().unwrap();
    token_worker.join().unwrap();
    // SAFETY: the server and token worker have stopped before this isolated test restores the environment.
    unsafe {
        if let Some(value) = previous {
            std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", value);
        } else {
            std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        }
    }
}

#[test]
#[ignore = "requires installed Codex CLI and isolated loopback access; run explicitly"]
fn installed_codex_roundtrips_a_vertex_tool_with_real_emp() {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let home = root.join("codex");
    let upstream = VertexFixture::start();
    let server = server(&root, &upstream);
    let settings = format!(
        "model = \"vertex/gemini-3-flash-preview\"\nmodel_provider = \"emp_fixture\"\nmodel_catalog_json = {}\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\n[model_providers.emp_fixture]\nname = \"EMP fixture\"\nbase_url = \"http://{}/v1\"\nwire_api = \"responses\"\nenv_key = \"EMP_TEST_KEY\"\nrequires_openai_auth = false\nhttp_headers = {{ \"X-EMP-Session\" = {} }}\n",
        serde_json::to_string(&home.join("models.json").to_string_lossy()).unwrap(),
        server.local_addr(),
        serde_json::to_string(&server.session_token()).unwrap()
    );
    std::fs::write(home.join("config.toml"), settings).unwrap();
    let log = home.join("executor.log");
    let stdout = std::fs::File::create(&log).unwrap();
    let stderr = stdout.try_clone().unwrap();
    let executor = OwnedChild::spawn(
        Command::new(std::env::var_os("EMP_TEST_CODEX_CLI").unwrap_or_else(|| "codex".into()))
            .args(["exec-server", "--listen", "ws://127.0.0.1:0"])
            .env("HOME", &home)
            .env("CODEX_HOME", &home)
            .current_dir(&home)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr),
    )
    .unwrap();
    let deadline = Deadline::after(test_timeout(Duration::from_secs(15)));
    let url = loop {
        let text = std::fs::read_to_string(&log).unwrap();
        if let Some((_, rest)) = text.split_once("ws://127.0.0.1:") {
            let port: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !port.is_empty() {
                break deadline
                    .accept(
                        format!("ws://127.0.0.1:{port}"),
                        "Codex exec-server started",
                    )
                    .unwrap();
            }
        }
        assert!(
            !deadline.is_expired(),
            "Codex exec-server did not start: {text}"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "Codex exec-server 通过日志报告地址，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(30));
    };
    let mut codex = Codex::start(&home);
    codex.rpc(1, "initialize", json!({"clientInfo":{"name":"emp_vertex_fixture","title":"EMP Vertex fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    codex.send(json!({"method":"initialized"}));
    codex.rpc(
        2,
        "environment/add",
        json!({"environmentId":"vertex-fixture","execServerUrl":url}),
    );
    let started = codex.rpc(
        3,
        "thread/start",
        json!({"model":"vertex/gemini-3-flash-preview","modelProvider":"emp_fixture",
        "cwd":home,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true}),
    );
    let thread = &started["thread"]["id"];
    codex.rpc(
        4,
        "turn/start",
        json!({"threadId":thread,"input":[{"type":"text","text":"Read the current directory."}],
        "environments":[{"environmentId":"vertex-fixture","cwd":home}]}),
    );
    let completed = codex.wait(|value| value["method"] == "turn/completed");
    assert_eq!(
        completed["params"]["turn"]["status"], "completed",
        "{completed}"
    );
    let first = upstream.received();
    let second = upstream.received();
    assert!(
        first["path"]
            .as_str()
            .unwrap()
            .contains("streamGenerateContent")
    );
    assert_signature(&second["body"]);
    drop(codex);
    drop(executor);
    server.shutdown().unwrap();
}
