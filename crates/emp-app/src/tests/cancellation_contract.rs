use super::*;

#[test]
fn downstream_disconnect_cancels_external_open_before_upstream_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    let (request_sender, request_received) = mpsc::sync_channel(1);
    let (probe, cancelled) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream");
        let _ = receive_upstream_request(&mut stream);
        request_sender.send(()).expect("announce upstream request");
        report_cancellation(&cancelled, |deadline| {
            upstream_closed_before(&stream, deadline)
        });
    });
    let (_directory, server) = configured_server(&format!("http://{address}/v1"));
    let body = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("request JSON");
    let downstream = open_post_stream(&server, "/v1/responses", &body, &[&session_header(&server)]);
    request_received
        .recv_timeout(Duration::from_secs(2))
        .expect("upstream request reached pending open");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    drop(downstream);
    assert!(
        wait_for_cancellation(&probe, &deadline),
        "EMP kept the upstream socket open while its downstream was disconnected before response headers"
    );
    server.shutdown().expect("shutdown");
    worker.join().expect("join upstream");
}
