use emp_test_support::test_timeout;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;

use super::{NATIVE_TOKEN, UpstreamRequest, http::read_message, upstream::read_request};

pub struct WebSocket {
    stream: TcpStream,
    masked: bool,
}

impl WebSocket {
    pub fn connect(port: u16) -> Self {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect Responses WS");
        stream
            .set_read_timeout(Some(test_timeout(Duration::from_secs(5))))
            .expect("WS handshake timeout");
        stream
            .set_write_timeout(Some(test_timeout(Duration::from_secs(5))))
            .expect("WS write timeout");
        write!(stream,
            "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer {NATIVE_TOKEN}\r\noriginator: codex_cli_rs\r\nthread-id: fixture-ws-thread\r\n\r\n"
        ).expect("WS handshake request");
        let (status, headers, _) = read_message(&mut stream, false);
        assert!(status.starts_with("HTTP/1.1 101 "), "{status}: {headers:?}");
        assert_eq!(
            headers["sec-websocket-accept"],
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        stream
            .set_read_timeout(Some(test_timeout(Duration::from_secs(5))))
            .expect("WS read timeout");
        Self {
            stream,
            masked: true,
        }
    }

    fn accept(mut stream: TcpStream) -> (Self, UpstreamRequest) {
        stream
            .set_read_timeout(Some(test_timeout(Duration::from_secs(5))))
            .expect("upstream WS handshake timeout");
        let request = read_request(&mut stream);
        assert_eq!(request.method, "GET");
        assert_eq!(request.headers["upgrade"].to_ascii_lowercase(), "websocket");
        assert_eq!(request.headers["sec-websocket-version"], "13");
        let key = format!(
            "{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11",
            request.headers["sec-websocket-key"]
        );
        let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, key.as_bytes());
        let accept = STANDARD.encode(digest.as_ref());
        write!(stream,
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        ).expect("upstream WS handshake");
        stream
            .set_read_timeout(Some(test_timeout(Duration::from_secs(5))))
            .expect("upstream WS timeout");
        stream
            .set_write_timeout(Some(test_timeout(Duration::from_secs(5))))
            .expect("upstream WS write timeout");
        (
            Self {
                stream,
                masked: false,
            },
            request,
        )
    }

    fn send_frame(&mut self, opcode: u8, finished: bool, payload: &[u8]) -> io::Result<()> {
        let mut frame = vec![opcode | if finished { 0x80 } else { 0 }];
        let mask_bit = if self.masked { 0x80 } else { 0 };
        match payload.len() {
            length @ 0..=125 => frame.push(mask_bit | length as u8),
            length @ 126..=65535 => {
                frame.push(mask_bit | 126);
                frame.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                frame.push(mask_bit | 127);
                frame.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        if self.masked {
            let mask = [5_u8, 6, 7, 8];
            frame.extend_from_slice(&mask);
            frame.extend(
                payload
                    .iter()
                    .enumerate()
                    .map(|(index, byte)| byte ^ mask[index % 4]),
            );
        } else {
            frame.extend_from_slice(payload);
        }
        self.stream.write_all(&frame)?;
        self.stream.flush()
    }

    pub fn send_json(&mut self, value: &Value) {
        let payload = value.to_string();
        let middle = payload.len() / 2;
        self.send_frame(1, false, &payload.as_bytes()[..middle])
            .expect("WS text fragment");
        self.send_frame(0, true, &payload.as_bytes()[middle..])
            .expect("WS continuation fragment");
    }

    pub fn next_json(&mut self) -> io::Result<Option<Value>> {
        let mut message = Vec::new();
        loop {
            let mut head = [0_u8; 2];
            match self.stream.read_exact(&mut head) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            }
            assert_eq!(head[0] & 0x70, 0, "unnegotiated WS extensions");
            let masked = head[1] & 0x80 != 0;
            assert_ne!(masked, self.masked, "WS sender mask");
            let mut length = u64::from(head[1] & 0x7f);
            if length == 126 {
                let mut bytes = [0_u8; 2];
                self.stream.read_exact(&mut bytes)?;
                length = u64::from(u16::from_be_bytes(bytes));
            } else if length == 127 {
                let mut bytes = [0_u8; 8];
                self.stream.read_exact(&mut bytes)?;
                length = u64::from_be_bytes(bytes);
            }
            let mut mask = [0_u8; 4];
            if masked {
                self.stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0_u8; usize::try_from(length).expect("WS frame length")];
            self.stream.read_exact(&mut payload)?;
            if masked {
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
            }
            match head[0] & 0x0f {
                0 | 1 => {
                    message.extend_from_slice(&payload);
                    if head[0] & 0x80 != 0 {
                        return serde_json::from_slice(&message)
                            .map(Some)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
                    }
                }
                8 => {
                    let _ = self.send_frame(8, true, &payload);
                    return Ok(None);
                }
                9 => self.send_frame(10, true, &payload)?,
                10 => {}
                opcode => panic!("unexpected WS opcode: {opcode}"),
            }
        }
    }

    pub fn close(&mut self) {
        self.send_frame(8, true, &1000_u16.to_be_bytes())
            .expect("client WS close");
    }
}

pub struct WebSocketUpstream {
    port: u16,
    handshakes: Receiver<UpstreamRequest>,
    turns: Receiver<Value>,
    closed: Receiver<()>,
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    worker: Option<JoinHandle<()>>,
}

impl WebSocketUpstream {
    pub fn start(replies: Vec<Vec<Value>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind WS upstream");
        let port = listener.local_addr().expect("WS upstream address").port();
        listener.set_nonblocking(true).expect("WS listener polling");
        let (handshake_sender, handshakes) = mpsc::channel();
        let (turn_sender, turns) = mpsc::channel();
        let (closed_sender, closed) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let worker_stop = Arc::clone(&stop);
        let worker_socket = Arc::clone(&socket);
        let worker = thread::spawn(move || {
            let stream = loop {
                if worker_stop.load(Ordering::Acquire) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                        )]
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept upstream WS: {error}"),
                }
            };
            *worker_socket.lock().expect("upstream socket") =
                Some(stream.try_clone().expect("clone upstream socket"));
            let (mut websocket, request) = WebSocket::accept(stream);
            handshake_sender
                .send(request)
                .expect("record upstream handshake");
            for events in replies {
                let turn = websocket
                    .next_json()
                    .expect("upstream WS request")
                    .expect("upstream closed before turn");
                turn_sender.send(turn).expect("record upstream turn");
                for event in events {
                    websocket.send_json(&event);
                }
            }
            assert!(
                websocket
                    .next_json()
                    .expect("wait for upstream WS close")
                    .is_none(),
                "unexpected extra WS turn"
            );
            let _ = closed_sender.send(());
        });
        Self {
            port,
            handshakes,
            turns,
            closed,
            stop,
            socket,
            worker: Some(worker),
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn handshake(&self) -> UpstreamRequest {
        self.handshakes
            .recv_timeout(test_timeout(Duration::from_secs(5)))
            .expect("upstream WS handshake")
    }

    pub fn next_turn(&self) -> Value {
        self.turns
            .recv_timeout(test_timeout(Duration::from_secs(5)))
            .expect("upstream WS turn")
    }

    pub fn assert_closed(&self) {
        self.closed
            .recv_timeout(test_timeout(Duration::from_secs(5)))
            .expect("downstream close must close upstream WS");
    }
}

impl Drop for WebSocketUpstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(socket) = self.socket.lock().expect("upstream socket").as_ref() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.expect("WS upstream worker");
            }
        }
    }
}
