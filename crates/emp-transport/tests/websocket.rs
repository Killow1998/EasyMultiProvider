use emp_transport::{ClientWebSocket, WebSocketConnection, websocket_accept};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::thread;
use std::time::Duration;

#[test]
fn websocket_accept_and_bidirectional_client_frames_match_rfc6455() {
    assert_eq!(
        websocket_accept("dGhlIHNhbXBsZSBub25jZQ==").unwrap(),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let key = head
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, value)| value.trim())
            })
            .unwrap();
        let accept = websocket_accept(key).unwrap();
        write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nOpenAI-Model: upstream\r\n\r\n").unwrap();
        stream.flush().unwrap();
        let mut websocket = WebSocketConnection::new(&mut stream);
        assert_eq!(
            websocket.receive_text().unwrap().unwrap(),
            "{\"hello\":true}"
        );
        websocket.send_json(&json!({"world":true})).unwrap();
    });
    let mut client = ClientWebSocket::connect(
        &format!("ws://{address}/v1/responses"),
        &Default::default(),
        Duration::from_secs(5),
    )
    .expect("client handshake");
    client.send_json(&json!({"hello":true})).unwrap();
    assert_eq!(
        client.receive_json().unwrap().unwrap(),
        json!({"world":true})
    );
    drop(client);
    worker.join().unwrap();
}
