//! The ignored host contract uses the installed CLI with a synthetic OAuth
//! store and a test executable wrapper that redirects inference to loopback.
use super::*;
use std::ffi::OsString;
use std::sync::MutexGuard;

const SYNTHETIC_ACCESS_TOKEN: &str = "sk-ant-oat01-SYNTHETIC-INVALID-DO-NOT-USE";
const SYNTHETIC_REFRESH_TOKEN: &str = "sk-ant-ort01-SYNTHETIC-INVALID-DO-NOT-USE";

static PROCESS_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy)]
enum AuthStatusBehavior {
    InstalledCli,
    ApiKeyFixture,
}

struct TestProcessEnvironment {
    previous: Vec<(OsString, Option<OsString>)>,
    _lock: MutexGuard<'static, ()>,
}

impl TestProcessEnvironment {
    fn install(wrapper_dir: &Path, home: &Path, config_dir: &Path) -> Self {
        let lock = PROCESS_ENVIRONMENT_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let keys = [
            "PATH",
            "HOME",
            "USERPROFILE",
            "CLAUDE_CONFIG_DIR",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "NODE_EXTRA_CA_CERTS",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "NODE_OPTIONS",
        ];
        let previous = keys
            .iter()
            .map(|key| (OsString::from(key), std::env::var_os(key)))
            .collect::<Vec<_>>();

        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(wrapper_dir.to_path_buf())
                .chain(std::env::split_paths(&inherited_path)),
        )
        .expect("test CLI PATH");
        let updates = [
            (OsString::from("PATH"), Some(path)),
            (OsString::from("HOME"), Some(home.as_os_str().to_owned())),
            (
                OsString::from("USERPROFILE"),
                Some(home.as_os_str().to_owned()),
            ),
            (
                OsString::from("CLAUDE_CONFIG_DIR"),
                Some(config_dir.as_os_str().to_owned()),
            ),
            (OsString::from("HTTP_PROXY"), None),
            (OsString::from("HTTPS_PROXY"), None),
            (OsString::from("ALL_PROXY"), None),
            (OsString::from("http_proxy"), None),
            (OsString::from("https_proxy"), None),
            (OsString::from("all_proxy"), None),
            (
                OsString::from("NO_PROXY"),
                Some(OsString::from("127.0.0.1,localhost,::1")),
            ),
            (
                OsString::from("no_proxy"),
                Some(OsString::from("127.0.0.1,localhost,::1")),
            ),
            (OsString::from("SSL_CERT_FILE"), None),
            (OsString::from("SSL_CERT_DIR"), None),
            (OsString::from("NODE_EXTRA_CA_CERTS"), None),
            (OsString::from("ANTHROPIC_API_KEY"), None),
            (OsString::from("ANTHROPIC_AUTH_TOKEN"), None),
            (OsString::from("ANTHROPIC_BASE_URL"), None),
            (OsString::from("CLAUDE_CODE_OAUTH_TOKEN"), None),
            (OsString::from("NODE_OPTIONS"), None),
        ];
        for (key, value) in updates {
            // SAFETY: these ignored host tests run alone through the serialized
            // Cargo wrapper; the lock also prevents overlapping cases here.
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(&key, value);
                } else {
                    std::env::remove_var(&key);
                }
            }
        }
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for TestProcessEnvironment {
    fn drop(&mut self) {
        for (key, value) in self.previous.iter().rev() {
            // SAFETY: the guard still holds the process-environment lock and
            // restores the state captured before this isolated test case.
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}

struct LocalRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct LocalMessagesApi {
    address: SocketAddr,
    requests: mpsc::Receiver<LocalRequest>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl LocalMessagesApi {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind local fake API");
        listener
            .set_nonblocking(true)
            .expect("nonblocking fake API listener");
        let address = listener.local_addr().expect("local fake API address");
        let (request_sender, requests) = mpsc::channel();
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !worker_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                if stream.set_nonblocking(false).is_err() {
                    break;
                }
                let Some(raw) = read_request_head(&mut stream) else {
                    break;
                };
                let method = raw
                    .head
                    .split_ascii_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                let (path, headers, body) = receive_upstream_request_from_head(&mut stream, raw);
                if request_sender
                    .send(LocalRequest {
                        method: method.clone(),
                        path: path.clone(),
                        headers,
                        body,
                    })
                    .is_err()
                {
                    break;
                }

                if path.split('?').next() == Some("/api/hello") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }
                if method != "POST" || path.split('?').next() != Some("/v1/messages") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    continue;
                }

                let response_body = local_messages_sse(&json!({
                    "answer":"synthetic local subscription response",
                    "tool_calls":[]
                }));
                let response_head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                let _ = stream.write_all(response_head.as_bytes());
                let _ = stream.write_all(&response_body);
                break;
            }
        });
        Self {
            address,
            requests,
            stopped,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn finish(mut self) -> Vec<LocalRequest> {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join local fake API");
        }
        self.requests.try_iter().collect()
    }
}

impl Drop for LocalMessagesApi {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn local_messages_sse(proposal: &Value) -> Vec<u8> {
    let partial = serde_json::to_string(proposal).expect("synthetic structured output");
    let events = [
        json!({"type":"message_start","message":{"id":"msg_synthetic","type":"message","role":"assistant","model":"sonnet","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_synthetic","name":"StructuredOutput","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":partial}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ];
    let mut output = Vec::new();
    for event in events {
        let name = event["type"].as_str().expect("event type");
        output.extend_from_slice(b"event: ");
        output.extend_from_slice(name.as_bytes());
        output.extend_from_slice(b"\ndata: ");
        output.extend_from_slice(&serde_json::to_vec(&event).expect("event JSON"));
        output.extend_from_slice(b"\n\n");
    }
    output
}

struct LocalLoginFixture {
    server: Option<ServerHandle>,
    upstream: Option<LocalMessagesApi>,
    _environment: TestProcessEnvironment,
    _directory: TempDir,
}

impl LocalLoginFixture {
    fn start(status: AuthStatusBehavior, with_oauth: bool, with_user_api_key_helper: bool) -> Self {
        let cli = emp_codex::installed_cli::resolve_claude_cli()
            .expect("installed trusted Claude Code CLI is required for this ignored host test");
        assert!(cli.executable.is_file());
        let guard = std::env::var_os("EMP_CLAUDE_TEST_EGRESS_GUARD")
            .map(PathBuf::from)
            .expect("set EMP_CLAUDE_TEST_EGRESS_GUARD to the local loopback-only guard");
        assert!(guard.is_file(), "loopback-only egress guard must exist");

        let directory = tempfile::tempdir().expect("temporary local-login fixture");
        let root = canonical_root(&directory);
        let home = root.join("home");
        let config_dir = home.join(".claude");
        let project = root.join("empty-project");
        let wrapper_dir = root.join("bin");
        for path in [
            &home,
            &config_dir,
            &project,
            &project.join(".claude"),
            &wrapper_dir,
        ] {
            std::fs::create_dir_all(path).expect("create private test directory");
        }
        std::fs::write(
            project.join(".claude/settings.json"),
            br#"{"disableAllHooks":true}"#,
        )
        .expect("write synthetic project settings");

        let api_key_helper_marker = root.join("api-key-helper-ran");
        if with_user_api_key_helper {
            let helper = root.join("api-key-helper.sh");
            std::fs::write(
                &helper,
                format!(
                    "#!/bin/sh\nprintf '%s' 'ran' > {}\nprintf '%s' 'synthetic-only-key'\n",
                    shell_quote(&api_key_helper_marker.to_string_lossy())
                ),
            )
            .expect("write local fake API-key helper");
            set_executable(&helper);
            let settings = json!({
                "apiKeyHelper": format!("/bin/sh {}", shell_quote(&helper.to_string_lossy()))
            });
            std::fs::write(
                config_dir.join("settings.json"),
                serde_json::to_vec(&settings).expect("synthetic user settings JSON"),
            )
            .expect("write synthetic user CPA helper setting");
        }
        if with_oauth {
            let credentials = json!({
                "claudeAiOauth": {
                    "accessToken": SYNTHETIC_ACCESS_TOKEN,
                    "refreshToken": SYNTHETIC_REFRESH_TOKEN,
                    "expiresAt": 4102444800000_u64,
                    "scopes": ["user:inference", "user:profile"],
                    "subscriptionType": "pro",
                    "rateLimitTier": "default_claude_pro"
                }
            });
            let path = config_dir.join(".credentials.json");
            std::fs::write(
                &path,
                serde_json::to_vec(&credentials).expect("synthetic OAuth JSON"),
            )
            .expect("write synthetic-only CLI auth store");
            set_private_file(&path);
        }

        let upstream = LocalMessagesApi::start();
        let wrapper = wrapper_dir.join("claude");
        let actual_cli = shell_quote(&cli.executable.to_string_lossy());
        let guard = shell_quote(&guard.to_string_lossy());
        let base_url_value = upstream.base_url();
        let base_url = shell_quote(&base_url_value);
        let status_command = match status {
            AuthStatusBehavior::InstalledCli => format!("exec {actual_cli} \"$@\""),
            AuthStatusBehavior::ApiKeyFixture => {
                "printf '%s\\n' '{\"loggedIn\":true,\"authMethod\":\"api_key\",\"apiProvider\":\"firstParty\"}'\nexit 0".to_owned()
            }
        };
        let wrapper_script = format!(
            "#!/bin/sh\nset -eu\nexport LD_PRELOAD={guard}\nif [ \"${{1-}}\" = \"--setting-sources\" ] && [ \"${{5-}}\" = \"auth\" ] && [ \"${{6-}}\" = \"status\" ]; then\n{status_command}\nfi\nexport ANTHROPIC_BASE_URL={base_url}\nexec {actual_cli} \"$@\"\n"
        );
        std::fs::write(&wrapper, wrapper_script).expect("write temporary test CLI wrapper");
        set_executable(&wrapper);

        let environment = TestProcessEnvironment::install(&wrapper_dir, &home, &config_dir);
        let resolved = emp_codex::installed_cli::resolve_claude_cli()
            .expect("trusted test executable wrapper must resolve first");
        assert_eq!(
            resolved.executable,
            wrapper.canonicalize().expect("wrapper path")
        );

        let config_path = root.join("emp-config.json");
        let config = json!({
            "providers": [{
                "id":"local-claude",
                "name":"Local Claude",
                "base_url":"",
                "protocol":"anthropic_messages",
                "auth_mode":"claude_login",
                "api_key":"",
                "api_key_file":"",
                "execution_backend":"claude_cli"
            }],
            "models": [{
                "id":"local-claude/sonnet",
                "provider":"local-claude",
                "upstream_id":"sonnet",
                "enabled":true
            }]
        });
        std::fs::write(
            &config_path,
            serde_json::to_vec(&config).expect("EMP test config"),
        )
        .expect("write EMP local-login config");
        let server =
            ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config_path)
                .expect("start isolated EMP server");

        Self {
            server: Some(server),
            upstream: Some(upstream),
            _environment: environment,
            _directory: directory,
        }
    }

    fn finish_upstream(&mut self) -> Vec<LocalRequest> {
        self.upstream
            .take()
            .map(LocalMessagesApi::finish)
            .unwrap_or_default()
    }

    fn finish(mut self) {
        if let Some(server) = self.server.take() {
            server.shutdown().expect("shutdown isolated EMP server");
        }
    }

    fn post_response(&self) -> String {
        let body = json!({
            "model":"local-claude/sonnet",
            "stream":false,
            "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic local OAuth endpoint test"}]}]
        });
        let server = self.server.as_ref().expect("test EMP server is running");
        post(
            server,
            "/v1/responses",
            &serde_json::to_vec(&body).expect("synthetic Responses request"),
            &[&session_header(server)],
        )
    }
}

impl Drop for LocalLoginFixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            // Drop must still attempt shutdown during assertion unwinding, but
            // must not panic a second time if lifecycle cleanup reports error.
            let _ = server.shutdown();
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("set private fixture executable mode");
}

#[cfg(unix)]
fn set_private_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .expect("set synthetic credential file mode");
}

fn response_value(response: &str) -> Value {
    serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("EMP HTTP response body")
            .1,
    )
    .expect("EMP JSON response")
}

fn request_status(response: &str) -> u16 {
    response
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("HTTP status")
}

fn assert_no_inference_requests(mut fixture: LocalLoginFixture) {
    assert!(
        fixture.finish_upstream().is_empty(),
        "auth preflight rejection must happen before any API request"
    );
    fixture.finish();
}

#[test]
#[ignore = "requires installed Claude Code CLI and EMP_CLAUDE_TEST_EGRESS_GUARD; run alone through the serialized Cargo wrapper"]
fn installed_local_oauth_completes_responses_endpoint_without_provider_key() {
    let mut fixture = LocalLoginFixture::start(AuthStatusBehavior::InstalledCli, true, true);
    let response = fixture.post_response();
    assert_eq!(request_status(&response), 200, "{response}");
    let result = response_value(&response);
    assert_eq!(result["status"], "completed");
    assert_eq!(result["model"], "local-claude/sonnet");
    assert_eq!(
        result["output"][0]["content"][0]["text"],
        "synthetic local subscription response"
    );
    assert!(
        !fixture
            ._directory
            .path()
            .join("api-key-helper-ran")
            .exists(),
        "user CPA apiKeyHelper must not run in status or project-only inference"
    );
    let requests = fixture.finish_upstream();
    let probe_count = requests
        .iter()
        .filter(|request| request.path.split('?').next() == Some("/api/hello"))
        .count();
    let inference = requests
        .iter()
        .filter(|request| {
            request.method == "POST" && request.path.split('?').next() == Some("/v1/messages")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        probe_count, 1,
        "expected one unauthenticated local CLI probe"
    );
    assert_eq!(
        requests.len(),
        2,
        "only the startup probe and one inference request are expected"
    );
    assert_eq!(inference.len(), 1, "expected exactly one inference request");
    assert!(
        inference[0]
            .headers
            .get("authorization")
            .is_some_and(|value| value == &format!("Bearer {SYNTHETIC_ACCESS_TOKEN}")),
        "inference must use only the synthetic OAuth bearer"
    );
    assert!(
        !inference[0].headers.contains_key("x-api-key"),
        "inference must not send an API key"
    );
    assert!(
        inference[0].body["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty()),
        "installed CLI must forward its resolved configured model"
    );
    assert!(inference[0].body["stream"].as_bool().unwrap_or(false));
    fixture.finish();
}

#[test]
#[ignore = "requires installed Claude Code CLI and EMP_CLAUDE_TEST_EGRESS_GUARD; run alone through the serialized Cargo wrapper"]
fn local_login_rejects_missing_cli_login_before_inference() {
    let fixture = LocalLoginFixture::start(AuthStatusBehavior::InstalledCli, false, false);
    let response = fixture.post_response();
    assert_eq!(request_status(&response), 401, "{response}");
    assert_eq!(
        response_value(&response)["error"]["code"],
        "claude_cli_login_required"
    );
    assert_no_inference_requests(fixture);
}

#[test]
#[ignore = "requires installed Claude Code CLI and EMP_CLAUDE_TEST_EGRESS_GUARD; run alone through the serialized Cargo wrapper"]
fn local_login_rejects_api_key_auth_status_before_inference() {
    let fixture = LocalLoginFixture::start(AuthStatusBehavior::ApiKeyFixture, false, false);
    let response = fixture.post_response();
    assert_eq!(request_status(&response), 403, "{response}");
    assert_eq!(
        response_value(&response)["error"]["code"],
        "claude_cli_subscription_required"
    );
    assert_no_inference_requests(fixture);
}
