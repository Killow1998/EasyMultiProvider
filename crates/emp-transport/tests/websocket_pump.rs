use emp_transport::{
    ClientWebSocket, ClientWebSocketPump, PumpCommand, PumpEvent, WebSocketPumpConfig,
    websocket_accept,
};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn handshake(stream: &mut TcpStream) {
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
    write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
    stream.flush().unwrap();
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
    stream.flush().unwrap();
}

fn read_masked_frame(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).unwrap();
    let mut length = usize::from(head[1] & 0x7f);
    if length == 126 {
        let mut raw = [0u8; 2];
        stream.read_exact(&mut raw).unwrap();
        length = usize::from(u16::from_be_bytes(raw));
    } else if length == 127 {
        let mut raw = [0u8; 8];
        stream.read_exact(&mut raw).unwrap();
        length = usize::try_from(u64::from_be_bytes(raw)).unwrap();
    }
    assert_ne!(head[1] & 0x80, 0, "client frames are masked");
    let mut mask = [0u8; 4];
    stream.read_exact(&mut mask).unwrap();
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % mask.len()];
    }
    (head[0] & 0x0f, payload)
}

#[test]
fn pump_preserves_raw_text_pongs_and_propagates_close_once() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let raw_upstream = " { \"kind\" : \"event\", \"n\" : 1 }\n";
    let raw_downstream = "{\"type\":\"cancel\", \"reason\":\"caller\"}";
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        handshake(&mut stream);
        write_server_frame(&mut stream, 0x89, b"keepalive");
        write_server_frame(&mut stream, 0x81, raw_upstream.as_bytes());

        let (opcode, pong) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 10, "the worker answers upstream ping itself");
        assert_eq!(pong, b"keepalive");

        let (opcode, text) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 1);
        assert_eq!(text, raw_downstream.as_bytes());

        write_server_frame(&mut stream, 0x88, &1000u16.to_be_bytes());
        let (opcode, close) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 8);
        assert_eq!(close, 1000u16.to_be_bytes());
    });

    let client = ClientWebSocket::connect(
        &format!("ws://{address}/sideband"),
        &Default::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    let mut pump = ClientWebSocketPump::spawn(client, WebSocketPumpConfig::default()).unwrap();
    match pump.recv_timeout(Duration::from_secs(2)).unwrap() {
        PumpEvent::Text(text) => assert_eq!(text, raw_upstream),
        event => panic!("unexpected pump event: {event:?}"),
    }
    pump.try_send(PumpCommand::Text(raw_downstream.to_owned()))
        .unwrap();
    assert_eq!(
        pump.recv_timeout(Duration::from_secs(2)).unwrap(),
        PumpEvent::Closed { code: Some(1000) }
    );
    let start = Instant::now();
    pump.join();
    assert!(start.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[test]
fn full_event_queue_does_not_block_close_or_leave_worker() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (sent_tx, sent_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        handshake(&mut stream);
        for index in 0..4 {
            let text = format!("{{\"event\":{index}}}");
            write_server_frame(&mut stream, 0x81, text.as_bytes());
        }
        sent_tx.send(()).unwrap();

        let (opcode, close) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 8);
        assert_eq!(&close[..2], &1001u16.to_be_bytes());
        assert_eq!(&close[2..], b"downstream closed");
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let mut extra = [0u8; 1];
        assert_eq!(
            stream.read(&mut extra).unwrap(),
            0,
            "only one close frame is sent"
        );
    });

    let client = ClientWebSocket::connect(
        &format!("ws://{address}/sideband"),
        &Default::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    let mut pump = ClientWebSocketPump::spawn(
        client,
        WebSocketPumpConfig {
            outbound_capacity: 1,
            inbound_capacity: 1,
            max_message_bytes: 4 * 1024 * 1024,
        },
    )
    .unwrap();
    sent_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    thread::sleep(Duration::from_millis(30));

    let start = Instant::now();
    pump.shutdown(1001, "downstream closed");
    assert!(start.elapsed() < Duration::from_secs(1));
    pump.join();
    server.join().unwrap();
}
