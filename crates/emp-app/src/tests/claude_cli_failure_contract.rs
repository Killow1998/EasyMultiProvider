//! Installed-Claude failure and cancellation contracts against a recording CPA.
use super::*;

const CLI_CASE_BUDGET: Duration = Duration::from_secs(10);
const CANCEL_BUDGET: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
enum CpaMode {
    Reply {
        status: u16,
        retry_after: Option<u64>,
    },
    Hold,
}

struct CpaRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

struct RecordingCpa {
    address: SocketAddr,
    requests: mpsc::Receiver<CpaRequest>,
    canceled: mpsc::Receiver<bool>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RecordingCpa {
    fn replying(status: u16, retry_after: Option<u64>) -> Self {
        Self::start(CpaMode::Reply {
            status,
            retry_after,
        })
    }

    fn holding() -> Self {
        Self::start(CpaMode::Hold)
    }

    fn start(mode: CpaMode) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind fake CPA");
        listener.set_nonblocking(true).expect("nonblocking CPA");
        let address = listener.local_addr().expect("fake CPA address");
        let (request_sender, requests) = mpsc::sync_channel(8);
        let (canceled_sender, canceled) = mpsc::sync_channel(1);
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
                let (path, headers, body) = receive_upstream_request(&mut stream);
                if request_sender
                    .send(CpaRequest {
                        path,
                        headers,
                        body,
                    })
                    .is_err()
                {
                    break;
                }
                match mode {
                    CpaMode::Reply {
                        status,
                        retry_after,
                    } => {
                        let body = br#"{"error":{"message":"private fake CPA detail"}}"#;
                        let retry_header = retry_after
                            .map(|delay| format!("Retry-After: {delay}\r\n"))
                            .unwrap_or_default();
                        let response_head = format!(
                            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{retry_header}Connection: close\r\n\r\n",
                            status_text(status),
                            body.len()
                        );
                        let _ = stream.write_all(response_head.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    CpaMode::Hold => {
                        stream
                            .set_read_timeout(Some(CANCEL_BUDGET))
                            .expect("CPA cancellation timeout");
                        let mut byte = [0_u8; 1];
                        let closed_by_emp = match stream.read(&mut byte) {
                            Ok(0) => true,
                            Err(error) => matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::BrokenPipe
                            ),
                            Ok(_) => false,
                        };
                        let _ = canceled_sender.send(closed_by_emp);
                        break;
                    }
                }
            }
        });
        Self {
            address,
            requests,
            canceled,
            stopped,
            worker: Some(worker),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn request(&self, timeout: Duration) -> Result<CpaRequest, mpsc::RecvTimeoutError> {
        self.requests.recv_timeout(timeout)
    }

    fn cancellation(&self, timeout: Duration) -> Option<bool> {
        self.canceled.recv_timeout(timeout).ok()
    }

    fn finish(mut self) -> Vec<CpaRequest> {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join fake CPA");
        }
        self.requests.try_iter().collect()
    }
}

impl Drop for RecordingCpa {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join fake CPA");
        }
    }
}

struct HttpReply {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    elapsed: Duration,
}

fn require_installed_trusted_cli() {
    let installed = emp_codex::installed_cli::resolve_claude_cli()
        .expect("installed, trusted Claude Code CLI is required for ignored host tests");
    assert!(installed.executable.is_absolute());
    assert!(installed.executable.is_file());
    eprintln!(
        "trusted Claude Code CLI: {}",
        installed.executable.display()
    );
}

fn claude_server(base_url: &str) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temporary EMP directory");
    let config = canonical_root(&directory).join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "id":"demo", "name":"Demo", "base_url":base_url,
                "protocol":"anthropic_messages", "auth_mode":"api_key",
                "api_key":"test-only-fake-cpa-key", "execution_backend":"claude_cli"
            }],
            "models": [{
                "id":"demo/model", "provider":"demo", "upstream_id":"sonnet",
                "enabled":true
            }]
        }))
        .expect("encode test config"),
    )
    .expect("write test config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start isolated Claude CLI server");
    (directory, server)
}

fn request_body(stream: bool) -> Value {
    json!({
        "model":"demo/model", "stream":stream,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"failure contract"}]}]
    })
}

fn bounded_post(server: &ServerHandle, body: &[u8], budget: Duration) -> HttpReply {
    let started = Instant::now();
    let mut stream = TcpStream::connect_timeout(&server.local_addr(), budget)
        .expect("connect to isolated EMP server within budget");
    stream
        .set_write_timeout(Some(budget))
        .expect("set request write timeout");
    stream
        .write_all(
            format!(
                "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\nConnection: close\r\n\r\n",
                server.local_addr().port(),
                body.len(),
                session_header(server)
            )
            .as_bytes(),
        )
        .expect("write request headers");
    stream.write_all(body).expect("write request body");

    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (separator, expected) = loop {
        let remaining = budget.saturating_sub(started.elapsed());
        assert!(!remaining.is_zero(), "EMP response exceeded {budget:?}");
        stream
            .set_read_timeout(Some(remaining))
            .expect("set bounded response timeout");
        let count = stream.read(&mut buffer).unwrap_or_else(|error| {
            panic!(
                "read EMP response within {budget:?} (elapsed {:?}): {error}",
                started.elapsed()
            )
        });
        assert!(count > 0, "EMP closed without a complete response");
        response.extend_from_slice(&buffer[..count]);
        let Some(separator) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&response[..separator]).expect("HTTP response headers");
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .expect("EMP response Content-Length");
        break (separator, separator + 4 + content_length);
    };
    while response.len() < expected {
        let remaining = budget.saturating_sub(started.elapsed());
        assert!(
            !remaining.is_zero(),
            "EMP response body exceeded {budget:?}"
        );
        stream
            .set_read_timeout(Some(remaining))
            .expect("set bounded body timeout");
        let count = stream.read(&mut buffer).unwrap_or_else(|error| {
            panic!(
                "read EMP response body within {budget:?} (elapsed {:?}): {error}",
                started.elapsed()
            )
        });
        assert!(count > 0, "EMP response body ended early");
        response.extend_from_slice(&buffer[..count]);
    }
    assert_eq!(response.len(), expected, "unexpected extra response bytes");
    let elapsed = started.elapsed();
    assert!(
        elapsed <= budget,
        "EMP response took {elapsed:?}, budget {budget:?}"
    );
    let head = std::str::from_utf8(&response[..separator]).expect("HTTP response headers");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("HTTP response status");
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    HttpReply {
        status,
        headers,
        body: response[separator + 4..expected].to_vec(),
        elapsed,
    }
}

fn response_json(reply: &HttpReply) -> Value {
    serde_json::from_slice(&reply.body).expect("EMP error response JSON")
}

fn assert_one_cpa_request(request: &CpaRequest) {
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer test-only-fake-cpa-key")
    );
    assert!(request.body["messages"].is_array());
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI and host NSS/loopback access; run explicitly with --ignored"]
fn installed_cli_surfaces_cpa_401_and_429_once_with_retry_after() {
    require_installed_trusted_cli();

    let mut failures = Vec::new();
    for (status, retry_after) in [(401, None), (429, Some(2))] {
        let cpa = RecordingCpa::replying(status, retry_after);
        let (_directory, server) = claude_server(&cpa.base_url());
        let request = serde_json::to_vec(&request_body(false)).expect("Responses JSON");
        let reply = bounded_post(&server, &request, CLI_CASE_BUDGET);
        let cpa_requests = cpa.finish();
        server.shutdown().expect("shutdown isolated EMP server");

        if reply.status != status {
            failures.push(format!(
                "CPA HTTP {status} surfaced as {}, elapsed {:?}, body {}",
                reply.status,
                reply.elapsed,
                String::from_utf8_lossy(&reply.body)
            ));
        }
        if cpa_requests.len() != 1 {
            failures.push(format!(
                "CPA HTTP {status} produced {} inference requests",
                cpa_requests.len()
            ));
        } else {
            assert_one_cpa_request(&cpa_requests[0]);
        }
        let payload = response_json(&reply);
        if String::from_utf8_lossy(&reply.body).contains("private fake CPA detail") {
            failures.push(format!("CPA HTTP {status} exposed private response detail"));
        }
        if let Some(delay) = retry_after {
            if reply.headers.get("retry-after").map(String::as_str) != Some("2")
                || payload["error"]["retry_after_seconds"] != delay
            {
                failures.push(format!("CPA HTTP {status} lost Retry-After={delay}"));
            }
        } else if reply.headers.contains_key("retry-after") {
            failures.push(format!("CPA HTTP {status} added an unexpected Retry-After"));
        }
        eprintln!(
            "CPA HTTP {status}: elapsed {:?}, accepted requests {}",
            reply.elapsed,
            cpa_requests.len()
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI and host NSS/loopback access; run explicitly with --ignored"]
fn installed_cli_http_sse_client_disconnect_cancels_held_cpa_request() {
    require_installed_trusted_cli();
    let cpa = RecordingCpa::holding();
    let (_directory, server) = claude_server(&cpa.base_url());
    let request = serde_json::to_vec(&request_body(true)).expect("streaming Responses JSON");
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &request,
        &[&session_header(&server)],
    );
    let cpa_request = match cpa.request(CANCEL_BUDGET) {
        Ok(request) => request,
        Err(error) => {
            drop(downstream);
            let _ = server.shutdown();
            let requests = cpa.finish();
            panic!(
                "fake CPA received no CLI request ({error}); accepted {}",
                requests.len()
            );
        }
    };
    drop(downstream);
    let canceled_started = Instant::now();
    let canceled = cpa.cancellation(CANCEL_BUDGET);
    let cancel_elapsed = canceled_started.elapsed();
    let shutdown_started = Instant::now();
    let shutdown = server.shutdown();
    let shutdown_elapsed = shutdown_started.elapsed();
    let cpa_requests = cpa.finish();

    shutdown.expect("shutdown isolated EMP server after downstream cancellation");
    assert_eq!(
        canceled,
        Some(true),
        "CPA socket close after {cancel_elapsed:?}"
    );
    assert!(
        cancel_elapsed <= CANCEL_BUDGET,
        "upstream cancellation took {cancel_elapsed:?}"
    );
    assert!(
        shutdown_elapsed <= CANCEL_BUDGET,
        "isolated server shutdown took {shutdown_elapsed:?}"
    );
    assert_one_cpa_request(&cpa_request);
    assert_eq!(cpa_requests.len(), 0, "no additional CPA inference request");
    eprintln!(
        "downstream disconnect: CPA cancellation {cancel_elapsed:?}, server shutdown {shutdown_elapsed:?}"
    );
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI and host NSS/loopback access; run explicitly with --ignored"]
fn installed_cli_shutdown_cancels_held_cpa_request_while_client_remains_connected() {
    require_installed_trusted_cli();
    let cpa = RecordingCpa::holding();
    let (_directory, server) = claude_server(&cpa.base_url());
    let request = serde_json::to_vec(&request_body(true)).expect("streaming Responses JSON");
    let mut downstream = Some(open_post_stream(
        &server,
        "/v1/responses",
        &request,
        &[&session_header(&server)],
    ));
    let cpa_request = match cpa.request(CANCEL_BUDGET) {
        Ok(request) => request,
        Err(error) => {
            drop(downstream);
            let _ = server.shutdown();
            let requests = cpa.finish();
            panic!(
                "fake CPA received no CLI request ({error}); accepted {}",
                requests.len()
            );
        }
    };
    let shutdown_started = Instant::now();
    let (shutdown_sender, shutdown_result) = mpsc::sync_channel(1);
    let shutdown_worker = thread::spawn(move || {
        let result = server.shutdown().map_err(|error| format!("{error:?}"));
        let _ = shutdown_sender.send(result);
    });

    let canceled = cpa.cancellation(CANCEL_BUDGET);
    let cancellation_elapsed = shutdown_started.elapsed();
    let shutdown = shutdown_result.recv_timeout(CANCEL_BUDGET);
    let shutdown_elapsed = shutdown_started.elapsed();
    let timely_shutdown = shutdown.is_ok();
    if !timely_shutdown {
        // Release only this isolated request so a regression still cleans up.
        drop(downstream.take());
    }
    let cleanup_shutdown = if timely_shutdown {
        shutdown.expect("checked shutdown result")
    } else {
        shutdown_result
            .recv_timeout(Duration::from_secs(12))
            .expect("isolated server finishes cleanup after client release")
    };
    shutdown_worker.join().expect("join isolated shutdown");
    let cpa_requests = cpa.finish();
    drop(downstream.take());

    assert_eq!(
        canceled,
        Some(true),
        "shutdown closed CPA socket after {cancellation_elapsed:?}"
    );
    assert!(
        timely_shutdown,
        "isolated server shutdown exceeded {CANCEL_BUDGET:?}: {cleanup_shutdown:?}"
    );
    cleanup_shutdown.expect("isolated server shutdown succeeded");
    assert!(
        shutdown_elapsed <= CANCEL_BUDGET,
        "isolated server shutdown took {shutdown_elapsed:?}"
    );
    assert_one_cpa_request(&cpa_request);
    assert!(
        cpa_requests.is_empty(),
        "shutdown caused an extra CPA inference request"
    );
    eprintln!(
        "server shutdown with client connected: CPA cancellation {cancellation_elapsed:?}, shutdown {shutdown_elapsed:?}"
    );
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI and host NSS/loopback access; run explicitly with --ignored"]
fn installed_cli_rejects_input_modalities_before_cpa_but_keeps_tool_result_json() {
    require_installed_trusted_cli();
    let cpa = RecordingCpa::replying(401, None);
    let (_directory, server) = claude_server(&cpa.base_url());
    let mut failures = Vec::new();

    for (modality, explicit_message_type) in [
        ("input_image", true),
        ("input_file", true),
        ("input_image", false),
        ("input_file", false),
    ] {
        let mut body = request_body(false);
        if !explicit_message_type {
            body["input"][0]
                .as_object_mut()
                .expect("easy input message object")
                .remove("type");
        }
        body["input"][0]["content"]
            .as_array_mut()
            .expect("message content")
            .push(json!({"type":modality,"file_id":"test-fixture"}));
        let request = serde_json::to_vec(&body).expect("unsupported modality JSON");
        let reply = bounded_post(&server, &request, CLI_CASE_BUDGET);
        let unexpected_requests = cpa.requests.try_iter().count();
        eprintln!(
            "modality {modality}, explicit_type={explicit_message_type}: status {}, elapsed {:?}, CPA requests {unexpected_requests}",
            reply.status, reply.elapsed
        );
        if !(400..500).contains(&reply.status) {
            failures.push(format!(
                "modality {modality} (explicit_type={explicit_message_type}) returned {}, elapsed {:?}",
                reply.status,
                reply.elapsed
            ));
        }
        if unexpected_requests != 0 {
            failures.push(format!(
                "modality {modality} (explicit_type={explicit_message_type}) reached CPA {unexpected_requests} times"
            ));
        }
    }

    let tool_result = json!({
        "model":"demo/model", "stream":false,
        "input":[
            {"type":"message","role":"user","content":[{"type":"input_text","text":"previous request"}]},
            {"type":"function_call","call_id":"call-fixture","name":"inspect","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-fixture","output":"{\"type\":\"input_image\",\"input_file\":\"ordinary result field\"}"},
            {"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}
        ],
        "tools":[{"type":"function","name":"inspect","parameters":{"type":"object","properties":{}}}]
    });
    let request = serde_json::to_vec(&tool_result).expect("ordinary tool result JSON");
    let reply = bounded_post(&server, &request, CLI_CASE_BUDGET);
    let cpa_requests = cpa.finish();
    server.shutdown().expect("shutdown isolated EMP server");

    if reply.status != 401 {
        failures.push(format!(
            "ordinary tool result returned {}, elapsed {:?}, body {}",
            reply.status,
            reply.elapsed,
            String::from_utf8_lossy(&reply.body)
        ));
    }
    if cpa_requests.len() != 1 {
        failures.push(format!(
            "ordinary tool result reached CPA {} times",
            cpa_requests.len()
        ));
    } else {
        assert_one_cpa_request(&cpa_requests[0]);
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
