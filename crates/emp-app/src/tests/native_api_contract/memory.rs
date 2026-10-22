use super::support::*;
use super::*;

#[test]
fn memory_websocket_pause_keeps_the_connection_usable_for_normal_turns() {
    let (_directory, server) =
        crate::tests::memory_routing_contract::fixture("http://127.0.0.1:9/v1");
    *server.state.backend.accounts.native_quota.lock().unwrap() =
        Some(json!({"rate_limits":{"primary":{"usedPercent":100}}}));
    let mut stream = TcpStream::connect(server.local_addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let cookie = session_header(&server);
    let mut request = format!("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\n\r\n",server.local_addr().port()).into_bytes();
    let mut memory = crate::tests::memory_routing_contract::body();
    memory["type"] = json!("response.create");
    request.extend_from_slice(&masked_websocket_text(&memory));
    stream.write_all(&request).unwrap();
    let mut handshake = Vec::new();
    while !handshake.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        handshake.push(byte[0]);
    }
    assert!(
        String::from_utf8(handshake)
            .unwrap()
            .starts_with("HTTP/1.1 101")
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "codex.response.metadata"
    );
    let error = receive_websocket_json(&mut stream);
    assert_eq!(error["status"], 503);
    assert_eq!(error["error"]["code"], "memory_routing_unavailable");
    send_masked_websocket_text(
        &mut stream,
        &json!({"type":"response.create","model":"gpt-6.1-sol","generate":false,"input":[]}),
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "codex.response.metadata"
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "response.created"
    );
    assert_eq!(
        receive_websocket_json(&mut stream)["type"],
        "response.completed"
    );
    drop(stream);
    server.shutdown().unwrap();
}
