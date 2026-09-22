use emp_transport::{ClientWebSocket, WebSocketConnection, websocket_accept};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
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

fn deflate(payload: &[u8]) -> Vec<u8> {
    let mut encoder = Compress::new(Compression::fast(), false);
    let mut output = Vec::with_capacity(payload.len() + 64);
    encoder
        .compress_vec(payload, &mut output, FlushCompress::Sync)
        .unwrap();
    assert!(output.ends_with(&[0, 0, 255, 255]));
    output.truncate(output.len() - 4);
    output
}

fn inflate(payload: &[u8]) -> Vec<u8> {
    let mut input = payload.to_vec();
    input.extend_from_slice(&[0, 0, 255, 255]);
    let mut decoder = Decompress::new(false);
    let mut output = Vec::with_capacity(4096);
    decoder
        .decompress_vec(&input, &mut output, FlushDecompress::Sync)
        .unwrap();
    output
}

fn read_masked_frame(stream: &mut impl Read) -> (u8, bool, Vec<u8>) {
    let mut head = [0_u8; 2];
    stream.read_exact(&mut head).unwrap();
    let mut length = usize::from(head[1] & 0x7f);
    if length == 126 {
        let mut raw = [0; 2];
        stream.read_exact(&mut raw).unwrap();
        length = usize::from(u16::from_be_bytes(raw));
    } else if length == 127 {
        let mut raw = [0; 8];
        stream.read_exact(&mut raw).unwrap();
        length = usize::try_from(u64::from_be_bytes(raw)).unwrap();
    }
    assert_ne!(head[1] & 0x80, 0);
    let mut mask = [0; 4];
    stream.read_exact(&mut mask).unwrap();
    let mut payload = vec![0; length];
    stream.read_exact(&mut payload).unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    (head[0] & 0x0f, head[0] & 0x40 != 0, payload)
}

fn write_server_frame(stream: &mut impl Write, first: u8, payload: &[u8]) {
    stream.write_all(&[first]).unwrap();
    if payload.len() < 126 {
        stream.write_all(&[payload.len() as u8]).unwrap();
    } else {
        stream.write_all(&[126]).unwrap();
        stream
            .write_all(&(payload.len() as u16).to_be_bytes())
            .unwrap();
    }
    stream.write_all(payload).unwrap();
}

#[test]
fn client_negotiates_deflate_and_reassembles_fragmented_upstream_json() {
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
        assert!(
            head.contains(
                "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n"
            )
        );
        let key = head
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, value)| value.trim())
            })
            .unwrap();
        let accept = websocket_accept(key).unwrap();
        write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Extensions: permessage-deflate; client_no_context_takeover; server_no_context_takeover\r\n\r\n").unwrap();
        stream.flush().unwrap();
        let (opcode, compressed, payload) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 1);
        assert!(compressed);
        assert_eq!(inflate(&payload), br#"{"hello":"compressed"}"#);
        let response = deflate(br#"{"world":"fragmented"}"#);
        let midpoint = response.len() / 2;
        write_server_frame(&mut stream, 0x41, &response[..midpoint]);
        write_server_frame(&mut stream, 0x80, &response[midpoint..]);
        stream.flush().unwrap();
    });
    let mut client = ClientWebSocket::connect(
        &format!("ws://{address}/v1/responses"),
        &Default::default(),
        Duration::from_secs(5),
    )
    .unwrap();
    client.send_json(&json!({"hello":"compressed"})).unwrap();
    assert_eq!(
        client.receive_json().unwrap(),
        Some(json!({"world":"fragmented"}))
    );
    drop(client);
    worker.join().unwrap();
}

#[test]
fn websocket_client_uses_the_frozen_http_proxy_route_and_credentials() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0; 1];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("GET ws://upstream.invalid/v1/responses?mode=ws HTTP/1.1\r\n"));
        assert!(head.contains("Host: upstream.invalid\r\n"));
        assert!(head.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
        assert!(head.contains("Authorization: Bearer target-secret\r\n"));
        let key = head
            .lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                    .map(|(_, value)| value.trim())
            })
            .unwrap();
        let accept = websocket_accept(key).unwrap();
        write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
        stream.flush().unwrap();
        let (opcode, compressed, payload) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 1);
        assert!(!compressed);
        assert_eq!(payload, br#"{"via":"proxy"}"#);
        write_server_frame(&mut stream, 0x81, br#"{"ok":true}"#);
        stream.flush().unwrap();
    });
    let headers = std::collections::BTreeMap::from([(
        "Authorization".to_owned(),
        "Bearer target-secret".to_owned(),
    )]);
    let proxy = format!("http://user:pass@{address}");
    let mut client = ClientWebSocket::connect_with_proxy(
        "ws://upstream.invalid/v1/responses?mode=ws",
        &headers,
        Duration::from_secs(5),
        Some(&proxy),
    )
    .unwrap();
    client.send_json(&json!({"via":"proxy"})).unwrap();
    assert_eq!(client.receive_json().unwrap(), Some(json!({"ok":true})));
    drop(client);
    worker.join().unwrap();
}
