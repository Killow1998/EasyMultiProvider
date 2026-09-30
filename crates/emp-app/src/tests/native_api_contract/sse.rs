use super::support::*;
use super::*;

struct NativeErrorSseUpstream {
    address: SocketAddr,
    worker: Option<JoinHandle<()>>,
}
impl NativeErrorSseUpstream {
    fn start(wire: Vec<u8>) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = receive_native_request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            for chunk in wire.chunks(13) {
                stream.write_all(chunk).unwrap();
                stream.flush().unwrap();
            }
        });
        Self {
            address,
            worker: Some(worker),
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
}
impl Drop for NativeErrorSseUpstream {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
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
        let wire = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[
                &cookie,
                "Authorization: Bearer caller",
                "thread-id: stream-thread",
                "x-openai-subagent: stream-subagent",
            ],
        );
        assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"), "{wire}");
        let (head, events) = wire.split_once("\r\n\r\n").unwrap();
        assert!(head.contains("openai-model: native/alias\r\n"), "{head}");
        if !ordinary {
            assert!(head.contains("x-codex-turn-state: stream-turn\r\n"));
            assert!(!head.contains("stale-stream-etag"));
            assert!(events.contains("response.created"));
            assert!(events.contains("response.output_text.delta"));
            assert!(events.contains("response.completed"));
            assert!(!events.contains("not-json"));
            assert!(events.contains("\"openai-model\":\"native/alias\""));
            assert!(events.contains("\"x-openai-model\":\"native/alias\""));
            assert!(events.contains("\"model\":\"upstream\""));
            assert!(events.contains("\"future\":{\"opaque\":true}"));
        } else {
            assert!(events.contains("response.created"));
            assert!(events.contains("response.completed"));
            assert!(events.contains("\"future\":\"kept\""));
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
            assert!(response.contains("response.output_text.delta"));
            assert!(response.contains("response.failed"));
            assert!(response.contains("stream_incomplete"));
        }
        if name == "failed" {
            assert!(response.contains("Retry-After: 3\r\n"));
        }
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_downstream_disconnect_cancels_open_before_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (request_sender, request_received) = mpsc::sync_channel(1);
    let (closed_sender, closed) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = receive_native_request(&mut stream);
        request_sender.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut byte = [0u8; 1];
        let ended = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        closed_sender.send(ended).unwrap();
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
    drop(downstream);
    assert!(
        closed.recv_timeout(Duration::from_secs(3)).unwrap(),
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
    let (closed_sender, closed) = mpsc::sync_channel(1);
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
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut byte = [0u8; 1];
        let ended = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        closed_sender.send(ended).unwrap();
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
    drop(downstream);
    assert!(
        closed.recv_timeout(Duration::from_secs(3)).unwrap(),
        "Rust EMP retained native upstream after Codex disconnected"
    );
    server.shutdown().unwrap();
    worker.join().unwrap();
}
