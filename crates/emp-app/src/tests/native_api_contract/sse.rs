use super::support::*;
use super::*;

struct NativeErrorSseUpstream {
    address: SocketAddr,
    worker: Option<JoinHandle<std::io::Result<usize>>>,
    expected_requests: usize,
}
impl NativeErrorSseUpstream {
    fn start(wire: Vec<u8>) -> Self {
        Self::start_repeated(wire, 1)
    }
    fn start_repeated(wire: Vec<u8>, requests: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            for received in 0..requests {
                let (mut stream, _) = listener.accept()?;
                if Deadline::after(crate::server::raw_tcp::wire_timeout())
                    .io(&stream)
                    .peek(&mut [0])?
                    == 0
                {
                    return Ok(received);
                }
                let _ = receive_native_request(&mut stream);
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")?;
                for chunk in wire.chunks(13) {
                    stream.write_all(chunk)?;
                    stream.flush()?;
                }
            }
            Ok(requests)
        });
        Self {
            address,
            worker: Some(worker),
            expected_requests: requests,
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn finish(mut self) {
        let received = finish_upstream(self.address, &mut self.worker)
            .expect("finish Native SSE fake upstream")
            .expect("Native SSE worker is running");
        assert_eq!(
            received, self.expected_requests,
            "Native SSE upstream request count"
        );
    }
}
impl Drop for NativeErrorSseUpstream {
    fn drop(&mut self) {
        let _ = finish_upstream(self.address, &mut self.worker);
    }
}

fn read_native_sse(input: impl BufRead) -> (String, Vec<String>) {
    let (head, mut stream) = read_sse_head(input);
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let mut frames = Vec::new();
    while !stream.fill_buf().unwrap().is_empty() {
        frames.push(read_sse_frame(&mut stream));
    }
    (head, frames)
}

fn post_native_sse(server: &ServerHandle, body: &[u8], headers: &[&str]) -> (String, Vec<String>) {
    let stream = open_post_stream(server, "/v1/responses", body, headers);
    read_native_sse(BufReader::new(
        Deadline::after(response_timeout()).io(stream),
    ))
}

fn incomplete_sse_event(frames: &[String]) -> Value {
    frames
        .iter()
        .flat_map(|frame| frame.lines().filter_map(|line| line.strip_prefix("data:")))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .find(|event| event["type"] == "response.incomplete")
        .expect("incomplete terminal")
}

#[test]
fn native_sse_finds_terminal_json_split_across_http_chunks() {
    let terminal =
        br#"data: {"type":"response.incomplete","response":{"usage":{"input_tokens":17}}}"#;
    let mut wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let mut event = terminal.to_vec();
    event.extend_from_slice(b"\n\n");
    for chunk in event.chunks(13) {
        write!(&mut wire, "{:x}\r\n", chunk.len()).unwrap();
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    let (_, frames) = read_native_sse(std::io::Cursor::new(wire));
    let decoded = incomplete_sse_event(&frames);
    assert_eq!(decoded["response"]["usage"]["input_tokens"], 17);
}

#[test]
fn native_sse_incomplete_preserves_usage_and_deduplicates_response_id() {
    for usage in [
        json!({"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}),
        Value::Null,
        json!({"input_tokens":"invalid","output_tokens":-1}),
    ] {
        let terminal = json!({"type":"response.incomplete","response":{
            "id":"resp_incomplete_usage", "object":"response", "status":"incomplete",
            "model":"upstream", "output":[], "usage":usage,
            "incomplete_details":{"reason":"max_output_tokens"}
        }});
        let wire = format!("data: {terminal}\n\n").into_bytes();
        let upstream = NativeErrorSseUpstream::start_repeated(wire, 2);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"fixture","stream":true}))
                .unwrap();
        for _ in 0..2 {
            let (_, frames) =
                post_native_sse(&server, &body, &[&cookie, "Authorization: Bearer caller"]);
            let event = incomplete_sse_event(&frames);
            assert_eq!(event["response"]["id"], terminal["response"]["id"]);
            assert_eq!(event["response"]["usage"], usage);
            assert_eq!(
                event["response"]["incomplete_details"],
                terminal["response"]["incomplete_details"]
            );
        }
        let now = crate::util::system_now();
        let totals = server
            .state
            .backend
            .usage
            .ledger
            .query(0.0, now, "all", now, None)
            .unwrap();
        assert_eq!(
            totals["totals"]["input_tokens"].as_u64().unwrap_or(0),
            usage["input_tokens"].as_u64().unwrap_or(0),
            "{totals}"
        );
        assert_eq!(
            totals["totals"]["output_tokens"].as_u64().unwrap_or(0),
            usage["output_tokens"].as_u64().unwrap_or(0),
            "{totals}"
        );
        upstream.finish();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_and_ordinary_json_cross_the_real_endpoint() {
    for ordinary in [false, true] {
        let upstream = NativeSseUpstream::start(ordinary);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let (head, frames) = post_native_sse(
            &server,
            &body,
            &[
                &cookie,
                "Authorization: Bearer caller",
                "thread-id: stream-thread",
                "x-openai-subagent: stream-subagent",
            ],
        );
        let has_event = |value| frames.iter().any(|frame| frame.contains(value));
        assert_eq!(
            response_header(&head, "openai-model"),
            Some("native/alias"),
            "{head}"
        );
        if !ordinary {
            assert_eq!(
                response_header(&head, "x-codex-turn-state"),
                Some("stream-turn"),
                "{head}"
            );
            assert!(!head.contains("stale-stream-etag"));
            assert!(has_event("response.created"));
            assert!(has_event("response.output_text.delta"));
            assert!(has_event("response.completed"));
            assert!(!has_event("not-json"));
            assert!(has_event("\"openai-model\":\"native/alias\""));
            assert!(has_event("\"x-openai-model\":\"native/alias\""));
            assert!(has_event("\"model\":\"upstream\""));
            assert!(has_event("\"future\":{\"opaque\":true}"));
        } else {
            assert!(has_event("response.created"));
            assert!(has_event("response.completed"));
            assert!(has_event("\"future\":\"kept\""));
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
        let cookie = session_header(&server);
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
            let (_, frames) = read_native_sse(response.as_bytes());
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.contains("response.output_text.delta"))
            );
            assert!(frames.iter().any(|frame| {
                frame.contains("response.failed") && frame.contains("stream_incomplete")
            }));
        }
        if name == "failed" {
            assert_eq!(
                response_header(response_parts(&response).0, "retry-after"),
                Some("3"),
                "{response}"
            );
        }
        upstream.finish();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_retry_headers_survive_pre_output_http_conversion() {
    for (headers, legacy, expected) in [
        (json!({"Retry-After":"3.1"}), None, 4),
        (json!({"rEtRy-AfTeR":"7"}), Some(3), 7),
        (json!({"Retry-After":"invalid"}), Some(3), 3),
    ] {
        let mut error = json!({"status":503,"error_class":"upstream_5xx","headers":headers});
        if let Some(delay) = legacy {
            error["retry_after_seconds"] = json!(delay);
        }
        let event = json!({"type":"response.failed","response":{"status":"failed","error":error}});
        let upstream = NativeErrorSseUpstream::start(format!("data: {event}\n\n").into_bytes());
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let response = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&cookie, "Authorization: Bearer caller"],
        );
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(
            response.contains(&format!("Retry-After: {expected}\r\n")),
            "{response}"
        );
        upstream.finish();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_downstream_disconnect_cancels_open_before_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_received) = mpsc::sync_channel(1);
    let (probe, cancelled) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = receive_native_request(&mut stream);
        request_sender.send(()).unwrap();
        report_cancellation(&cancelled, |deadline| {
            upstream_closed_before(&stream, deadline)
        });
    });
    let (_directory, server) = native_alias_server(&format!("http://{address}/v1"));
    let cookie = session_header(&server);
    let body = serde_json::to_vec(&json!({
        "model":"native/alias", "input":"hello", "stream":true
    }))
    .unwrap();
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    request_received
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    drop(downstream);
    assert!(
        wait_for_cancellation(&probe, &deadline),
        "Rust EMP retained native upstream while open_stream awaited response headers"
    );
    server.shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn native_sse_downstream_disconnect_cancels_upstream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (head_sender, head_ready) = mpsc::sync_channel(1);
    let (probe, cancelled) = mpsc::channel();
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
        report_cancellation(&cancelled, |deadline| {
            upstream_closed_before(&stream, deadline)
        });
    });
    let (_directory, server) = native_alias_server(&format!("http://{address}/v1"));
    let cookie = session_header(&server);
    let body =
        serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true})).unwrap();
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    head_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    drop(downstream);
    assert!(
        wait_for_cancellation(&probe, &deadline),
        "Rust EMP retained native upstream after Codex disconnected"
    );
    server.shutdown().unwrap();
    worker.join().unwrap();
}

#[test]
fn responses_sse_filters_codex_extensions_only_for_generic_clients() {
    for codex in [false, true] {
        let wire = [
            json!({"type":"codex.rate_limits","rate_limits":{}}),
            json!({"type":"codex.response.metadata","headers":{}}),
            json!({"type":"response.metadata","headers":{}}),
            json!({"type":"keepalive"}),
            json!({"type":"response.output_text.delta","delta":"hello"}),
            json!({"type":"codex.rate_limits","rate_limits":{"after_output":true}}),
            json!({"type":"response.completed","response":{"id":"extension-test","status":"completed","output":[]}}),
        ].iter().map(|v| format!("data: {v}\n\n")).collect::<String>();
        let upstream = NativeErrorSseUpstream::start(wire.into_bytes());
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let (_, frames) = post_native_sse(
            &server,
            br#"{"model":"native/alias","input":"hello","stream":true}"#,
            &[
                &session_header(&server),
                "Authorization: Bearer fixture",
                if codex {
                    "originator: codex_cli_rs"
                } else {
                    "User-Agent: async-openai"
                },
            ],
        );
        let has_event = |value| frames.iter().any(|frame| frame.contains(value));
        assert!(has_event("response.output_text.delta"), "{frames:?}");
        assert!(has_event("response.completed"), "{frames:?}");
        for extension in [
            "codex.rate_limits",
            "codex.response.metadata",
            "response.metadata",
            "\"type\":\"keepalive\"",
        ] {
            assert_eq!(has_event(extension), codex, "{frames:?}");
        }
        if !codex {
            assert!(frames.iter().any(|frame| frame == ": keepalive\n"));
        }
        upstream.finish();
        server.shutdown().unwrap();
    }
}
