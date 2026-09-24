use super::*;
use crate::services::connection_admission::{ConnectionAdmissionConfig, SlotLimits};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

fn server_with_admission(admission: ConnectionAdmissionConfig) -> (TempDir, ServerHandle) {
    let directory = tempfile::tempdir().expect("temporary root");
    let config = canonical_root(&directory).join("config.json");
    let server = ServerHandle::start_with_connection_admission_for_test(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        admission,
    )
    .expect("start server with test admission limits");
    (directory, server)
}

fn wait_for(timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready(), "condition did not become ready before deadline");
}

fn websocket(server: &ServerHandle) -> TcpStream {
    let mut stream = TcpStream::connect(server.local_addr()).expect("connect websocket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set websocket timeout");
    let cookie = session_cookie_header(server);
    write!(
        stream,
        "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{cookie}\r\n\r\n",
        server.local_addr().port()
    )
    .expect("send websocket handshake");
    stream.flush().expect("flush websocket handshake");
    let mut head = Vec::new();
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        stream
            .read_exact(&mut byte)
            .expect("read websocket handshake");
        head.push(byte[0]);
    }
    assert!(
        String::from_utf8(head)
            .expect("HTTP handshake text")
            .starts_with("HTTP/1.1 101"),
        "downstream websocket upgrade must complete before overload close"
    );
    stream
}

fn read_server_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut header = [0_u8; 2];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            return None;
        }
        Err(error) => panic!("read websocket frame header: {error}"),
    }
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut length = usize::from(header[1] & 0x7f);
    if length == 126 {
        let mut raw = [0_u8; 2];
        stream.read_exact(&mut raw).expect("read frame length");
        length = usize::from(u16::from_be_bytes(raw));
    } else if length == 127 {
        let mut raw = [0_u8; 8];
        stream.read_exact(&mut raw).expect("read frame length");
        length = usize::try_from(u64::from_be_bytes(raw)).expect("frame length fits usize");
    }
    let mask = if masked {
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask).expect("read frame mask");
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .expect("read websocket frame payload");
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % mask.len()];
        }
    }
    Some((opcode, payload))
}

fn send_masked_text(stream: &mut TcpStream, value: &Value) {
    let payload = serde_json::to_vec(value).expect("websocket JSON");
    let mask = [11_u8, 12, 13, 14];
    let mut frame = vec![0x81];
    match payload.len() {
        length if length < 126 => frame.push(0x80 | length as u8),
        length if length <= u16::MAX as usize => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    stream.write_all(&frame).expect("send websocket event");
    stream.flush().expect("flush websocket event");
}

fn health_status(server: &ServerHandle) -> Option<String> {
    let mut stream = TcpStream::connect(server.local_addr()).expect("connect health endpoint");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("set health timeout");
    let write_request = write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        server.local_addr().port()
    );
    if let Err(error) = write_request {
        if health_request_disconnected(&error) {
            return None;
        }
        panic!("send health request: {error}");
    }
    if let Err(error) = stream.flush() {
        if health_request_disconnected(&error) {
            return None;
        }
        panic!("flush health request: {error}");
    }
    let mut status = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                status.push(byte[0]);
                if status.ends_with(b"\r\n") {
                    return Some(String::from_utf8(status).expect("HTTP status text"));
                }
            }
            Err(error) if health_request_disconnected(&error) => return None,
            Err(error) => panic!("read health response: {error}"),
        }
    }
}

fn health_request_disconnected(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

fn stall_request(server: &ServerHandle) -> TcpStream {
    let mut stream = TcpStream::connect(server.local_addr()).expect("connect stalled request");
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost:")
        .expect("send incomplete request head");
    stream.flush().expect("flush incomplete request head");
    stream
}

fn tiny_admission(requests: usize, websockets: usize) -> ConnectionAdmissionConfig {
    ConnectionAdmissionConfig {
        requests: SlotLimits {
            initial: requests,
            maximum: requests,
            growth: 1,
        },
        websockets: SlotLimits {
            initial: websockets,
            maximum: websockets,
            growth: 1,
        },
    }
}

#[test]
fn websocket_admission_closes_overflow_and_reuses_a_released_slot() {
    let (_directory, server) = server_with_admission(tiny_admission(8, 2));
    let first = websocket(&server);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_websockets() == 1
    });
    let second = websocket(&server);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_websockets() == 2
    });
    let health_while_websockets_full = health_status(&server);

    let mut overflow = websocket(&server);
    overflow
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set overflow timeout");
    let (opcode, payload) = read_server_frame(&mut overflow).expect("overload close frame");
    let overflow_code = if opcode == 8 && payload.len() >= 2 {
        Some(u16::from_be_bytes([payload[0], payload[1]]))
    } else {
        None
    };
    drop(overflow);

    drop(first);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_websockets() == 1
    });
    let mut replacement = websocket(&server);
    send_masked_text(
        &mut replacement,
        &json!({"type":"response.create","model":"missing/model"}),
    );
    let (opcode, payload) = read_server_frame(&mut replacement).expect("admitted response frame");
    let first_event: Value = serde_json::from_slice(&payload).expect("server websocket JSON");
    drop(replacement);
    drop(second);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_websockets() == 0
    });
    server.shutdown().expect("shutdown test server");

    assert_eq!(overflow_code, Some(1013));
    assert_eq!(opcode, 1);
    assert_eq!(first_event["type"], "codex.response.metadata");
    assert_eq!(
        health_while_websockets_full.as_deref(),
        Some("HTTP/1.1 200 OK\r\n")
    );
}

#[test]
fn request_admission_rejects_overflow_and_releases_after_disconnect() {
    let (_directory, server) = server_with_admission(tiny_admission(2, 2));
    let first = stall_request(&server);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_requests() == 1
    });
    let second = stall_request(&server);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_requests() == 2
    });

    let overflow_health = health_status(&server);
    drop(first);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_requests() == 1
    });
    let available_health = health_status(&server);
    drop(second);
    wait_for(Duration::from_secs(2), || {
        server.state.connection_admission.active_requests() == 0
    });
    server.shutdown().expect("shutdown test server");

    assert_eq!(
        overflow_health, None,
        "the saturated listener closes overflow"
    );
    assert_eq!(available_health.as_deref(), Some("HTTP/1.1 200 OK\r\n"));
}
