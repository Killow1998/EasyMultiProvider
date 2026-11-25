use emp_test_support::{Deadline, Observation, ObservationResult, accept_blocking, test_timeout};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
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
        let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect Responses WS");
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        let mut io = deadline.io(&stream);
        write!(io,
            "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer {NATIVE_TOKEN}\r\noriginator: codex_cli_rs\r\nthread-id: fixture-ws-thread\r\n\r\n"
        ).expect("WS handshake request");
        let (status, headers, _) = read_message(&mut io, false).expect("read WS handshake");
        assert!(status.starts_with("HTTP/1.1 101 "), "{status}: {headers:?}");
        assert_eq!(
            headers["sec-websocket-accept"],
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        Self {
            stream,
            masked: true,
        }
    }

    fn accept(stream: TcpStream, deadline: &Deadline) -> (Self, UpstreamRequest) {
        let request = read_request(&mut deadline.io(&stream));
        assert_eq!(request.method, "GET");
        assert_eq!(request.headers["upgrade"].to_ascii_lowercase(), "websocket");
        assert_eq!(request.headers["sec-websocket-version"], "13");
        let key = format!(
            "{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11",
            request.headers["sec-websocket-key"]
        );
        let digest = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, key.as_bytes());
        let accept = STANDARD.encode(digest.as_ref());
        write!(deadline.io(&stream),
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        ).expect("upstream WS handshake");
        (
            Self {
                stream,
                masked: false,
            },
            request,
        )
    }

    fn send_frame(
        &self,
        opcode: u8,
        finished: bool,
        payload: &[u8],
        deadline: &Deadline,
    ) -> io::Result<()> {
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
        let mut stream = deadline.io(&self.stream);
        stream.write_all(&frame)?;
        stream.flush()
    }

    pub fn send_json(&mut self, value: &Value, deadline: &Deadline) {
        let payload = value.to_string();
        let middle = payload.len() / 2;
        self.send_frame(1, false, &payload.as_bytes()[..middle], deadline)
            .expect("WS text fragment");
        self.send_frame(0, true, &payload.as_bytes()[middle..], deadline)
            .expect("WS continuation fragment");
    }

    pub fn next_json(&mut self, deadline: &Deadline) -> io::Result<Option<Value>> {
        let mut stream = deadline.io(&self.stream);
        let mut message = Vec::new();
        loop {
            let mut head = [0_u8; 2];
            match stream.read_exact(&mut head) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    return deadline.accept(None, "WebSocket EOF");
                }
                Err(error) => return Err(error),
            }
            assert_eq!(head[0] & 0x70, 0, "unnegotiated WS extensions");
            let masked = head[1] & 0x80 != 0;
            assert_ne!(masked, self.masked, "WS sender mask");
            let mut length = u64::from(head[1] & 0x7f);
            if length == 126 {
                let mut bytes = [0_u8; 2];
                stream.read_exact(&mut bytes)?;
                length = u64::from(u16::from_be_bytes(bytes));
            } else if length == 127 {
                let mut bytes = [0_u8; 8];
                stream.read_exact(&mut bytes)?;
                length = u64::from_be_bytes(bytes);
            }
            let mut mask = [0_u8; 4];
            if masked {
                stream.read_exact(&mut mask)?;
            }
            let mut payload = vec![0_u8; usize::try_from(length).expect("WS frame length")];
            stream.read_exact(&mut payload)?;
            if masked {
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
            }
            match head[0] & 0x0f {
                0 | 1 => {
                    message.extend_from_slice(&payload);
                    if head[0] & 0x80 != 0 {
                        let value = serde_json::from_slice(&message)
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        return deadline.accept(Some(value), "WebSocket message");
                    }
                }
                8 => {
                    let _ = self.send_frame(8, true, &payload, deadline);
                    return deadline.accept(None, "WebSocket close");
                }
                9 => self.send_frame(10, true, &payload, deadline)?,
                10 => {}
                opcode => panic!("unexpected WS opcode: {opcode}"),
            }
        }
    }

    pub fn close(&mut self, deadline: &Deadline) {
        self.send_frame(8, true, &1000_u16.to_be_bytes(), deadline)
            .expect("client WS close");
    }
}

pub struct WebSocketUpstream {
    port: u16,
    observation: Observation<Option<ObservedTurn>>,
}

pub struct ObservedTurn {
    pub handshake: Option<UpstreamRequest>,
    pub request: Value,
}

impl WebSocketUpstream {
    pub fn start(replies: Vec<Vec<Value>>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind WS upstream");
        let port = listener.local_addr().expect("WS upstream address").port();
        listener.set_nonblocking(true).expect("WS listener polling");
        let observation = Observation::spawn(move |probe| {
            let Some(mut pending) = probe.next_request() else {
                return;
            };
            let stream = loop {
                pending.deadline().accept((), "accept upstream WS").unwrap();
                match accept_blocking(&listener) {
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
            let (mut websocket, request) = WebSocket::accept(stream, pending.deadline());
            let mut handshake = Some(request);
            for events in replies {
                let deadline = *pending.deadline();
                let request = websocket
                    .next_json(&deadline)
                    .expect("upstream WS request")
                    .expect("upstream closed before turn");
                pending.complete(Some(ObservedTurn {
                    handshake: handshake.take(),
                    request,
                }));
                for event in events {
                    websocket.send_json(&event, &deadline);
                }
                let Some(next) = probe.next_request() else {
                    return;
                };
                pending = next;
            }
            assert!(
                websocket
                    .next_json(pending.deadline())
                    .expect("wait for upstream WS close")
                    .is_none(),
                "unexpected extra WS turn"
            );
            pending.complete(None);
        });
        Self { port, observation }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn begin(
        &self,
        deadline: Deadline,
        context: &'static str,
    ) -> ObservationResult<Option<ObservedTurn>> {
        self.observation.begin(deadline, context)
    }
}
