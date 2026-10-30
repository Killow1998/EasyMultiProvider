//! Installed-CLI history compaction and its recording upstream fixture.
use super::media::minimal_pdf;
use super::*;

fn claude_summary_server_with_context(
    base_url: &str,
    context_window: u64,
) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "providers": [{
                "id":"demo", "name":"Demo", "base_url":base_url,
                "protocol":"auto", "auth_mode":"api_key",
                "api_key":"upstream-secret", "execution_backend":"claude_cli"
            }],
            "models": [
                {
                    "id":"demo/long", "provider":"demo", "upstream_id":"long-context-model",
                    "enabled":true, "context_window":100000, "output_limit":256,
                    "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}
                },
                {
                    "id":"demo/short", "provider":"demo", "upstream_id":"sonnet",
                    "enabled":true, "context_window":context_window, "output_limit":64,
                    "capability_sources":{"context_window":{"source":"manual","confidence":1.0}}
                }
            ]
        }))
        .expect("encode config"),
    )
    .expect("write config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start short-destination server");
    (directory, server)
}

struct SummaryRequest {
    path: String,
    headers: BTreeMap<String, String>,
    body: Value,
    transcript: Value,
    final_turn: bool,
}

struct SummaryUpstream {
    address: std::net::SocketAddr,
    requests: mpsc::Receiver<SummaryRequest>,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SummaryUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind summary CPA");
        listener
            .set_nonblocking(true)
            .expect("nonblocking summary CPA");
        let address = listener.local_addr().expect("summary CPA address");
        let (sender, requests) = mpsc::channel();
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = std::sync::Arc::clone(&stopped);
        let worker = thread::spawn(move || {
            while !worker_stopped.load(std::sync::atomic::Ordering::Acquire) {
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
                    .expect("normalize accepted summary CPA socket");
                let (path, headers, body) = receive_upstream_request(&mut stream);
                let transcript = user_transcript(&body);
                let content = match &body["messages"][0]["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let final_turn = content.contains("active destination request");
                if sender
                    .send(SummaryRequest {
                        path,
                        headers,
                        body,
                        transcript,
                        final_turn,
                    })
                    .is_err()
                {
                    break;
                }
                thread::sleep(Duration::from_millis(200));
                let answer = if final_turn {
                    "short destination answer"
                } else {
                    "portable checkpoint"
                };
                let response_body = structured_messages_sse(&json!({
                    "answer":answer,
                    "tool_calls":[]
                }));
                let response_head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                if stream.write_all(response_head.as_bytes()).is_err()
                    || stream.write_all(&response_body).is_err()
                {
                    break;
                }
                if final_turn {
                    break;
                }
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
        format!("http://{}/v1", self.address)
    }

    fn requests_through_final(&self) -> Vec<SummaryRequest> {
        let deadline = Instant::now() + Duration::from_secs(9);
        let mut requests = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "short-destination inference did not arrive"
            );
            let request = self
                .requests
                .recv_timeout(remaining)
                .expect("bounded summary CPA request");
            let final_turn = request.final_turn;
            requests.push(request);
            if final_turn {
                return requests;
            }
        }
    }
}

impl Drop for SummaryUpstream {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join summary CPA");
        }
    }
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_compacts_history_above_the_model_context_before_inference() {
    assert!(emp_codex::installed_cli::resolve_claude_cli().is_some());
    let upstream = SummaryUpstream::start();
    let (_directory, server) = claude_summary_server_with_context(&upstream.base_url(), 1_000_000);
    let mut input = (0..48)
        .map(|index| {
            json!({
                "type":"message", "role":"user",
                "content":format!("visible history {index}: {}", "h".repeat(256 * 1024))
            })
        })
        .collect::<Vec<_>>();
    let active = vec![
        json!({"type":"message","role":"user","content":"active destination request"}),
        json!({"type":"function_call","name":"read","call_id":"active-call","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":"active-call","output":"active result"}),
    ];
    input.extend(active.iter().cloned());
    let body = serde_json::to_vec(&json!({
        "model":"demo/short", "stream":false, "max_output_tokens":4096,
        "instructions":"preserve the current turn and its tool pair", "input":input,
        "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]
    }))
    .unwrap();
    assert!(body.len() > 10 * 1024 * 1024);
    // Decode the entire history before applying the model's context budget.
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&body).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() < 64 * 1024);
    let started = Instant::now();
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    write!(stream, "POST /v1/responses HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n{}\r\nConnection: close\r\n\r\n", server.local_addr(), compressed.len(), session_header(&server)).unwrap();
    stream.write_all(&compressed).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(started.elapsed() < Duration::from_secs(60));
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(
        crate::services::compaction::response_output_text(&result).as_deref(),
        Some("short destination answer")
    );
    let requests = upstream.requests_through_final();
    assert!((2..=16).contains(&requests.len()));
    for request in &requests {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.body["model"], "sonnet");
        assert!(serde_json::to_vec(&request.body).unwrap().len() < 12 * 1024 * 1024);
        assert_eq!(request.body["tools"][0]["name"], "StructuredOutput");
        assert_eq!(request.body["tools"].as_array().unwrap().len(), 1);
    }
    let final_request = requests.last().unwrap();
    let final_input = final_request.transcript["input"].as_array().unwrap();
    assert_eq!(&final_input[final_input.len() - active.len()..], &active);
    assert!(
        final_input[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("portable checkpoint")
    );
    assert!(
        final_request.body["system"]
            .to_string()
            .contains("preserve the current turn and its tool pair")
    );
    eprintln!(
        "Claude adapter regression: {} encoded bytes / {} decoded bytes -> {} bounded CLI requests -> completed",
        compressed.len(),
        body.len(),
        requests.len()
    );
    server
        .shutdown()
        .expect("shutdown large-history Claude test server");
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_preserves_media_above_ten_mib_without_forcing_compaction() {
    assert!(emp_codex::installed_cli::resolve_claude_cli().is_some());
    let upstream = SummaryUpstream::start();
    let (_directory, server) = claude_server(&upstream.base_url());
    let mut image = super::media::synthetic_png(32, 32, false);
    image.resize(10 * 1024 * 1024, 0);
    let image_url = format!("data:image/png;base64,{}", STANDARD.encode(&image));
    let input = json!([
        {"role":"user","content":[{"type":"input_image","image_url":image_url}]},
        {"type":"function_call","name":"read","call_id":"large-call","arguments":"{}"},
        {"type":"function_call_output","call_id":"large-call","output":"complete result"},
        {"role":"user","content":"active destination request"}
    ]);
    let body = serde_json::to_vec(&json!({
        "model":"demo/model","stream":false,"input":input,
        "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]
    }))
    .unwrap();
    let response = post(&server, "/v1/responses", &body, &[&session_header(&server)]);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let requests = upstream.requests_through_final();
    assert_eq!(requests.len(), 1);
    assert!(serde_json::to_vec(&requests[0].body).unwrap().len() > 12 * 1024 * 1024);
    let blocks = requests[0].body["messages"][0]["content"]
        .as_array()
        .unwrap();
    let native_image = blocks
        .iter()
        .find(|block| block["type"] == "image")
        .unwrap();
    assert!(native_image["source"]["data"] == STANDARD.encode(&image));
    let history = blocks
        .iter()
        .filter_map(|block| block["text"].as_str())
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .collect::<Vec<_>>();
    assert_eq!(history[2]["item"], input[1]);
    assert_eq!(history[3]["item"], input[2]);
    assert_eq!(history[4]["item"], input[3]);
    server.shutdown().unwrap();
}

#[test]
#[ignore = "requires an installed trusted Claude Code CLI on PATH"]
fn installed_cli_handles_bounded_long_history_compaction_at_short_destination() {
    assert!(
        emp_codex::installed_cli::resolve_claude_cli().is_some(),
        "trusted Claude Code CLI must be on PATH"
    );

    let upstream = SummaryUpstream::start();
    let (_directory, server) = claude_summary_server_with_context(&upstream.base_url(), 5000);
    let mut input = Vec::new();
    let pdf_bytes = minimal_pdf();
    let pdf = format!(
        "data:application/pdf;base64,{}",
        STANDARD.encode(&pdf_bytes)
    );
    for turn in 0..4 {
        let mut content = vec![json!({
            "type":"input_text",
            "text":format!("long-context user turn {turn}: {}", "u".repeat(2400))
        })];
        if turn == 0 {
            content.push(json!({
                "type":"input_file","filename":"checkpoint.pdf","file_data":pdf
            }));
        }
        input.push(json!({"type":"message","role":"user","content":content}));
        input.push(json!({
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":format!("long-context assistant turn {turn}: {}", "a".repeat(2400))}]
        }));
    }
    input.push(json!({
        "type":"message", "role":"user",
        "content":[{"type":"input_text","text":"active destination request"}]
    }));
    let body = serde_json::to_vec(&json!({
        "model":"demo/short",
        "stream":false,
        "max_output_tokens":64,
        "input":input
    }))
    .expect("long-context request JSON");

    let started = Instant::now();
    let response = post(&server, "/v1/responses", &body, &[&session_header(&server)]);
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "bounded destination summary and completion"
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let result: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1)
        .expect("destination Responses body");
    assert_eq!(
        crate::services::compaction::response_output_text(&result).as_deref(),
        Some("short destination answer")
    );

    let requests = upstream.requests_through_final();
    assert!(
        (2..=5).contains(&requests.len()),
        "expected bounded map/reduce summaries plus one destination inference, got {}",
        requests.len()
    );
    for request in &requests {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.headers["authorization"], "Bearer upstream-secret");
        assert!(request.headers.contains_key("anthropic-version"));
        assert_eq!(request.body["model"], "sonnet");
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(request.body["tools"][0]["name"], "StructuredOutput");
    }
    assert!(
        requests[..requests.len() - 1].iter().any(|request| request
            .transcript
            .to_string()
            .contains("Create a structured portable checkpoint")),
        "a summary transcript must reach the actual CLI Messages request"
    );
    assert!(
        requests[..requests.len() - 1].iter().any(|request| {
            request.body["messages"][0]["content"]
                .as_array()
                .is_some_and(|content| {
                    content.iter().any(|block| {
                        block["type"] == "document"
                            && block["source"]["media_type"] == "application/pdf"
                            && block["source"]["data"] == STANDARD.encode(&pdf_bytes)
                    })
                })
        }),
        "automatic short-destination compaction must preserve native PDF bytes"
    );
    let final_request = requests.last().expect("final destination inference");
    assert!(final_request.final_turn);
    assert!(
        final_request
            .transcript
            .to_string()
            .contains("portable checkpoint"),
        "the compacted checkpoint must reach the short destination inference"
    );
    assert_eq!(
        final_request.transcript["model"], "demo/short",
        "the configured short destination remains the client model"
    );

    server
        .shutdown()
        .expect("shutdown short-destination server");
}
