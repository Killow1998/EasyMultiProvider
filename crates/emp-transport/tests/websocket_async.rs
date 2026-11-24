use emp_transport::{AsyncClientWebSocket, WebSocketEvent, websocket_accept};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

fn handshake(stream: &mut TcpStream, extensions: &str) {
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
    write!(stream,"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n{extensions}\r\n").unwrap();
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
    (head[0], payload)
}

#[tokio::test]
async fn async_session_preserves_raw_text_pongs_and_propagates_close_once() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let raw_upstream = " { \"kind\" : \"event\", \"n\" : 1 }\n";
    let raw_downstream = "{\"type\":\"cancel\", \"reason\":\"caller\"}";
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        handshake(&mut stream, "");
        write_server_frame(&mut stream, 0x89, b"keepalive");
        write_server_frame(&mut stream, 0x81, raw_upstream.as_bytes());

        let (opcode, pong) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 0x8a, "the async session answers upstream ping");
        assert_eq!(pong, b"keepalive");

        let (opcode, text) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 0x81);
        assert_eq!(text, raw_downstream.as_bytes());

        write_server_frame(&mut stream, 0x88, &1000u16.to_be_bytes());
        let (opcode, close) = read_masked_frame(&mut stream);
        assert_eq!(opcode, 0x88);
        assert_eq!(close, 1000u16.to_be_bytes());
        assert_eq!(
            stream.read(&mut [0]).unwrap(),
            0,
            "only one close frame is sent"
        );
    });

    let mut client = AsyncClientWebSocket::connect_with_proxy(
        &format!("ws://{address}/sideband"),
        &Default::default(),
        Duration::from_secs(3),
        None,
    )
    .await
    .unwrap();
    let WebSocketEvent::Ping(payload) = client.read().await.unwrap() else {
        panic!("expected upstream ping");
    };
    client.send_pong(&payload).await.unwrap();
    assert_eq!(
        client.read().await.unwrap(),
        WebSocketEvent::Text(raw_upstream.to_owned())
    );
    client.send_text(raw_downstream).await.unwrap();
    assert_eq!(
        client.read().await.unwrap(),
        WebSocketEvent::Closed {
            code: Some(1000),
            payload: Vec::from(1000u16.to_be_bytes())
        }
    );
    let start = Instant::now();
    client.close(1000, "").await;
    client.close(1000, "").await;
    drop(client);
    assert!(start.elapsed() < Duration::from_secs(1));
    server.join().unwrap();
}

#[tokio::test]
async fn negotiated_windows_keep_deflate_context_in_both_directions_across_turns() {
    use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let text: String = (0..1800)
        .scan(13_u32, |state, _| {
            *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            Some(char::from(b'!' + (*state >> 24) as u8 % 90))
        })
        .collect();
    let expected = text.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        handshake(
            &mut stream,
            "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits=12; server_max_window_bits=12\r\n",
        );
        let mut inflate = Decompress::new_with_window_bits(false, 12);
        let mut deflate = Compress::new_with_window_bits(Compression::fast(), false, 12);
        for turn in 0..2 {
            let (first, mut payload) = read_masked_frame(&mut stream);
            assert_eq!(first, 0xc1);
            if turn == 1 {
                assert!(
                    payload.len() < 128,
                    "second client message must reuse its dictionary"
                );
            }
            payload.extend_from_slice(&[0, 0, 255, 255]);
            let mut decoded = Vec::with_capacity(4096);
            inflate
                .decompress_vec(&payload, &mut decoded, FlushDecompress::Sync)
                .unwrap();
            assert_eq!(decoded, expected.as_bytes());

            let mut compressed = Vec::with_capacity(4096);
            deflate
                .compress_vec(expected.as_bytes(), &mut compressed, FlushCompress::Sync)
                .unwrap();
            assert!(compressed.ends_with(&[0, 0, 255, 255]));
            if turn == 1 {
                assert!(
                    compressed.len() < 128,
                    "the upstream also uses its previous dictionary"
                );
            }
            let payload = &compressed[..compressed.len() - 4];
            let midpoint = payload.len() / 2;
            write_server_frame(&mut stream, 0x41, &payload[..midpoint]);
            write_server_frame(&mut stream, 0x89, b"between fragments");
            write_server_frame(&mut stream, 0x80, &payload[midpoint..]);
            assert_eq!(
                read_masked_frame(&mut stream),
                (0x8a, b"between fragments".to_vec())
            );
        }
        assert_eq!(
            read_masked_frame(&mut stream),
            (0x88, 1000u16.to_be_bytes().to_vec())
        );
    });
    let mut client = AsyncClientWebSocket::connect_with_proxy(
        &format!("ws://{address}/responses"),
        &Default::default(),
        Duration::from_secs(3),
        None,
    )
    .await
    .unwrap();
    assert!(client.compression_enabled());
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..2 {
            client.send_text(&text).await.unwrap();
            let WebSocketEvent::Ping(payload) = client.read().await.unwrap() else {
                panic!("expected ping before the fragmented message");
            };
            assert_eq!(payload, b"between fragments");
            client.send_pong(&payload).await.unwrap();
            assert_eq!(
                client.read().await.unwrap(),
                WebSocketEvent::Text(text.clone())
            );
        }
        client.close(1000, "").await;
    })
    .await
    .unwrap();
    server.join().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn tcp_reset_during_frame_write_returns_failure_and_unknown_delivery() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let socket = socket2::SockRef::from(&stream);
        socket.set_recv_buffer_size(4096).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        handshake(&mut stream, "");
        let mut prefix = [0; 14];
        stream.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix[..2], &[0x81, 0xff]);
        socket2::SockRef::from(&stream)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
    });
    let mut client = AsyncClientWebSocket::connect_with_proxy(
        &format!("ws://{address}/responses"),
        &Default::default(),
        Duration::from_secs(5),
        None,
    )
    .await
    .unwrap();
    let error = client
        .send_text(&"x".repeat(8 * 1024 * 1024))
        .await
        .unwrap_err();
    assert_eq!(error.status(), 502);
    assert!(matches!(
        client.traffic(),
        emp_transport::WebSocketTraffic {
            decoded_sent: None,
            encoded_sent: None,
            ..
        }
    ));
    server.join().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn tcp_write_deadline_is_reported_without_confirming_partial_frame() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let (finished, release) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        socket2::SockRef::from(&stream)
            .set_recv_buffer_size(4096)
            .unwrap();
        handshake(&mut stream, "");
        release.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let mut client = AsyncClientWebSocket::connect_with_proxy(
        &format!("ws://{address}/responses"),
        &Default::default(),
        Duration::from_secs(2),
        None,
    )
    .await
    .unwrap();
    let error = client
        .send_text(&"x".repeat(16 * 1024 * 1024))
        .await
        .unwrap_err();
    assert_eq!(error.status(), 502);
    assert!(error.to_string().contains("TimedOut"));
    assert_eq!(client.traffic().decoded_sent, None);
    finished.send(()).unwrap();
    server.join().unwrap();
}
