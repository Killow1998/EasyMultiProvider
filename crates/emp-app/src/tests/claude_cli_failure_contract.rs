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
    HoldThenReply,
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
            let mut held = false;
            while !worker_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                stream
                    .set_nonblocking(false)
                    .expect("normalize accepted fake CPA socket");
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
                if matches!(mode, CpaMode::HoldThenReply) && held {
                    let body = super::claude_cli_contract::structured_messages_sse(
                        &json!({"answer":"after steering","tool_calls":[]}),
                    );
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(&body);
                    continue;
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
                    CpaMode::Hold | CpaMode::HoldThenReply => {
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
                        if matches!(mode, CpaMode::HoldThenReply) {
                            held = true;
                        } else {
                            break;
                        }
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
fn installed_cli_websocket_steering_stops_inference_and_continues_without_restarting_the_connection()
 {
    require_installed_trusted_cli();
    let cpa = RecordingCpa::start(CpaMode::HoldThenReply);
    let (directory, server) = claude_server(&cpa.base_url());
    let mut client = emp_transport::ClientWebSocket::connect(
        &format!("ws://{}/v1/responses", server.local_addr()),
        &BTreeMap::from([("X-EMP-Session".into(), server.session_token().to_owned())]),
        CANCEL_BUDGET,
    )
    .unwrap();
    client
        .readiness_stream()
        .unwrap()
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let event = |client: &mut emp_transport::ClientWebSocket, kind: &str| {
        let deadline = Instant::now() + CLI_CASE_BUDGET;
        loop {
            assert!(Instant::now() < deadline, "waiting for {kind}");
            match client.poll_receive_text().unwrap() {
                emp_transport::WebSocketPoll::Text(text) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    if event["type"] == kind {
                        break event;
                    }
                    assert_ne!(event["type"], "error", "{event}");
                    assert_ne!(event["type"], "response.failed", "{event}");
                }
                emp_transport::WebSocketPoll::Pending => {}
                other => panic!("{other:?}"),
            }
        }
    };
    client.send_json(&json!({"type":"response.create","model":"demo/model","input":"first","reasoning":{"effort":"low"}})).unwrap();
    let created = event(&mut client, "response.created");
    let id = created["response"]["id"].as_str().unwrap();
    assert_one_cpa_request(&cpa.request(CANCEL_BUDGET).unwrap());
    let interrupt =
        json!({"type":"response.interrupt","response_id":id,"mode":"discard_partial_items"});
    let started = Instant::now();
    client.send_json(&interrupt).unwrap();
    let incomplete = event(&mut client, "response.incomplete");
    assert_eq!(incomplete["response"]["id"], id);
    assert_eq!(
        incomplete["response"]["incomplete_details"]["reason"],
        "interrupted"
    );
    assert!(started.elapsed() < CANCEL_BUDGET);
    assert_eq!(cpa.cancellation(CANCEL_BUDGET), Some(true));
    client.send_json(&json!({"type":"response.create","model":"demo/model","input":"after","previous_response_id":id})).unwrap();
    assert_eq!(
        event(&mut client, "error")["error"]["code"],
        "previous_response_not_found"
    );
    client.send_json(&interrupt).unwrap(); // late control does not restart inference.
    client.send_json(&json!({"type":"response.create","model":"demo/model","input":"after","reasoning":{"effort":"low"}})).unwrap();
    let next = event(&mut client, "response.created");
    assert_ne!(next["response"]["id"], id);
    assert_eq!(
        event(&mut client, "response.completed")["response"]["id"],
        next["response"]["id"]
    );
    assert_one_cpa_request(&cpa.request(CANCEL_BUDGET).unwrap());
    client.close();
    drop(client);
    let calls = server
        .state
        .backend
        .usage
        .ledger
        .query_calls(&emp_state::usage::ledger::CallFilter {
            start: 0.0,
            end: crate::util::system_now() + 1.0,
            category: None,
            provider: None,
            account: None,
            model: None,
            models: vec![],
            session: None,
            state: Some("interrupted".into()),
            request: None,
            offset: 0,
            limit: 10,
            models_offset: 0,
            models_sort: "calls".into(),
        })
        .unwrap();
    assert_eq!(
        calls["records"].as_array().unwrap().len(),
        1,
        "steering must be an interruption, not a provider failure"
    );
    let activity = server
        .state
        .backend
        .activity
        .snapshot(crate::util::system_now() as u64);
    assert!(
        activity["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|request| request["state"] == "interrupted")
    );
    server.shutdown().unwrap();
    assert!(
        cpa.finish().is_empty(),
        "steering must not retry the cancelled inference"
    );
    let events = super::internal_events_contract::journal(directory.path());
    let failure = events
        .iter()
        .find(|event| event["event"] == "claude_cli_request_failed")
        .unwrap();
    assert_eq!(failure["fields"]["error_code"], "claude_cli_interrupted");
    assert_eq!(failure["fields"]["stage"], "cancelled");
    let cancelled = events
        .iter()
        .find(|event| event["event"] == "model_request_cancelled")
        .unwrap();
    assert_eq!(cancelled["fields"]["error_class"], "client_cancelled");
    assert_eq!(cancelled["fields"]["failure_reason"], "interrupted");
    let control = events
        .iter()
        .find(|event| {
            event["event"] == "websocket_control"
                && event["fields"]["accepted"] == true
                && event["fields"]["late"] == false
        })
        .unwrap();
    assert_eq!(
        control["fields"]["request_id"],
        failure["fields"]["request_id"]
    );
    let receipt = events
        .iter()
        .find(|event| {
            event["event"] == "route_observation"
                && event["fields"]["failure_reason"] == "interrupted"
        })
        .unwrap();
    assert_eq!(receipt["fields"]["error_class"], "client_cancelled");
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI and host NSS/loopback access; run explicitly with --ignored"]
fn installed_cli_reports_output_budget_exhaustion_without_waiting_or_retrying() {
    require_installed_trusted_cli();
    for (json_response, stream) in [(false, false), (false, true), (true, false)] {
        let message = json!({"type":"message","id":"msg_budget","role":"assistant",
            "model":"sonnet","content":[{"type":"thinking","thinking":"private synthetic thinking"}],
            "stop_reason":"max_tokens","usage":{"input_tokens":2,"output_tokens":256}});
        let cpa = if json_response {
            OneShotUpstream::start(message)
        } else {
            let events = [
                json!({"type":"message_start","message":{"type":"message","id":"msg_budget","role":"assistant","model":"sonnet","content":[],"usage":{"input_tokens":2}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private synthetic thinking"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":256}}),
                json!({"type":"message_stop"}),
            ];
            let sse = events
                .iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect::<String>();
            OneShotUpstream::start_sse(sse.as_bytes().chunks(11).map(<[u8]>::to_vec).collect())
        };
        let (directory, server) = claude_server(&cpa.base_url());
        let mut body = request_body(stream);
        body["max_output_tokens"] = json!(256);
        body["reasoning"] = json!({"effort":"low"});
        let reply = bounded_post(
            &server,
            &serde_json::to_vec(&body).unwrap(),
            CLI_CASE_BUDGET,
        );
        assert_eq!(reply.status, 502);
        let error = response_json(&reply);
        assert_eq!(error["error"]["code"], "claude_cli_output_budget_exhausted");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("max_output_tokens")
        );
        let (_, _, forwarded) = cpa.observed();
        assert_eq!(
            forwarded["max_tokens"], 256,
            "never silently raise the caller budget"
        );
        assert_eq!(forwarded["output_config"]["effort"], "low");
        assert!(
            cpa.observed.try_recv().is_err(),
            "no automatic extra inference"
        );
        server.shutdown().expect("shutdown isolated budget server");
        let events = super::internal_events_contract::journal(directory.path());
        let failures = events
            .iter()
            .filter(|event| event["event"] == "claude_cli_request_failed")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0]["fields"]["error_code"],
            "claude_cli_output_budget_exhausted"
        );
        assert_eq!(failures[0]["fields"]["stage"], "output_budget");
        assert_eq!(failures[0]["fields"]["error_origin"], "claude_cli");
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("private synthetic thinking")
        );
    }
}

#[test]
#[ignore = "requires trusted installed Claude Code CLI and host NSS/loopback access; run explicitly with --ignored"]
fn installed_cli_surfaces_cpa_401_and_429_once_with_retry_after() {
    require_installed_trusted_cli();

    let mut failures = Vec::new();
    for (status, retry_after) in [(401, None), (429, Some(2))] {
        let cpa = RecordingCpa::replying(status, retry_after);
        let (directory, server) = claude_server(&cpa.base_url());
        let request = serde_json::to_vec(&request_body(false)).expect("Responses JSON");
        let reply = bounded_post(&server, &request, CLI_CASE_BUDGET);
        let cpa_requests = cpa.finish();
        server.shutdown().expect("shutdown isolated EMP server");
        let events = super::internal_events_contract::journal(directory.path());
        let failures_logged = events
            .iter()
            .filter(|event| event["event"] == "claude_cli_request_failed")
            .collect::<Vec<_>>();
        assert_eq!(failures_logged.len(), 1);
        assert_eq!(failures_logged[0]["fields"]["status"], status);
        assert!(failures_logged[0]["fields"]["error_code"].is_string());
        assert!(failures_logged[0]["fields"]["duration_ms"].is_u64());
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("private fake CPA detail")
        );

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
fn claude_cli_rejects_unresolvable_media_before_cpa_inference() {
    let cpa = RecordingCpa::replying(401, None);
    let (_directory, server) = claude_server(&cpa.base_url());
    let mut failures = Vec::new();

    for (label, modality, explicit_message_type, media_fields) in [
        (
            "unresolved image file ID",
            "input_image",
            true,
            json!({"file_id":"file-unavailable"}),
        ),
        (
            "unresolved document file ID in shorthand message",
            "input_file",
            false,
            json!({"file_id":"file-unavailable"}),
        ),
        (
            "local document path reference",
            "input_file",
            true,
            json!({"filename":"private.txt","file_url":"/private/fixture.txt"}),
        ),
        (
            "audio input",
            "input_audio",
            true,
            json!({"input_audio":{"data":"AAAA","format":"wav"}}),
        ),
        (
            "video input",
            "input_video",
            true,
            json!({"video_url":"https://video.invalid/sample.mp4"}),
        ),
    ] {
        let mut body = request_body(false);
        if !explicit_message_type {
            body["input"][0]
                .as_object_mut()
                .expect("easy input message object")
                .remove("type");
        }
        let mut part = json!({"type":modality});
        if let Some(fields) = media_fields.as_object() {
            for (key, value) in fields {
                part[key] = value.clone();
            }
        }
        body["input"][0]["content"]
            .as_array_mut()
            .expect("message content")
            .push(part);
        let request = serde_json::to_vec(&body).expect("unsupported modality JSON");
        let reply = bounded_post(&server, &request, CLI_CASE_BUDGET);
        let unexpected_requests = cpa.requests.try_iter().count();
        eprintln!(
            "{label}: status {}, elapsed {:?}, CPA requests {unexpected_requests}",
            reply.status, reply.elapsed
        );
        if !(400..500).contains(&reply.status) {
            failures.push(format!(
                "{label} returned {}, elapsed {:?}",
                reply.status, reply.elapsed
            ));
        }
        if unexpected_requests != 0 {
            failures.push(format!("{label} reached CPA {unexpected_requests} times"));
        }
        let error: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
        let error_text = error.to_string().to_ascii_lowercase();
        if !error_text.contains("unsupported") && !error_text.contains("file") {
            failures.push(format!(
                "{label} did not return an actionable media error: {error}"
            ));
        }
    }

    let cpa_requests = cpa.finish();
    server.shutdown().expect("shutdown isolated EMP server");
    if !cpa_requests.is_empty() {
        failures.push(format!(
            "unsupported media reached CPA {} times",
            cpa_requests.len()
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}
