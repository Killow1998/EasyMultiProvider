use super::*;

#[test]
fn downstream_disconnect_cancels_external_open_before_upstream_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    let (request_sender, request_received) = mpsc::sync_channel(1);
    let (closed_sender, closed) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept upstream");
        let _ = receive_upstream_request(&mut stream);
        request_sender.send(()).expect("announce upstream request");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("upstream close timeout");
        let mut byte = [0_u8; 1];
        let closed_by_emp = match stream.read(&mut byte) {
            Ok(0) => true,
            Err(error) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ),
            Ok(_) => false,
        };
        closed_sender
            .send(closed_by_emp)
            .expect("report open cancellation");
    });
    let (_directory, server) = configured_server(&format!("http://{address}/v1"));
    let body = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("request JSON");
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&session_cookie_header(&server)],
    );
    request_received
        .recv_timeout(Duration::from_secs(2))
        .expect("upstream request reached pending open");
    drop(downstream);
    assert!(
        closed
            .recv_timeout(Duration::from_secs(3))
            .expect("upstream open cancellation result"),
        "EMP kept the upstream socket open while its downstream was disconnected before response headers"
    );
    server.shutdown().expect("shutdown");
    worker.join().expect("join upstream");
}
