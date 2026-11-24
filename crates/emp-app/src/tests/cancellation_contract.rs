use super::*;

#[test]
fn downstream_disconnect_cancels_external_open_before_upstream_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind upstream");
    let address = listener.local_addr().expect("upstream address");
    listener.set_nonblocking(true).unwrap();
    let observation = Observation::spawn(move |probe| {
        let Some(dispatch) = probe.next_request() else {
            return;
        };
        let mut stream = accept_before(&listener, dispatch.deadline());
        let _ = receive_upstream_request(&mut stream);
        dispatch.complete(true);
        probe.observe(|deadline| upstream_closed_before(&stream, deadline));
    });
    let (_directory, server) = configured_server(&format!("http://{address}/v1"));
    let body = serde_json::to_vec(&json!({
        "model":"demo/model", "stream":true,
        "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
    }))
    .expect("request JSON");
    let dispatched = observation.begin(
        Deadline::after(test_timeout(Duration::from_secs(2))),
        "upstream request reached pending open",
    );
    let downstream = open_post_stream(&server, "/v1/responses", &body, &[&session_header(&server)]);
    assert!(dispatched.wait().unwrap());
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    let cancellation = observation.begin(deadline, "upstream cancellation");
    drop(downstream);
    assert!(
        cancellation.wait().unwrap(),
        "EMP kept the upstream socket open while its downstream was disconnected before response headers"
    );
    server.shutdown().expect("shutdown");
    drop(observation);
}
