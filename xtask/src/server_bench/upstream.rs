use super::wire::{self, Head, IO_TIMEOUT, Socket};
use serde_json::{Value, json};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) const HTTP_ID: &str = "resp_bench_http";
pub(super) const WS_ID: &str = "resp_bench_ws";

pub(super) struct Upstream {
    pub(super) port: u16,
    pub(super) gated: Receiver<Cancellation>,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl Upstream {
    pub(super) fn start() -> io::Result<Self> {
        let listener = wire::bind_high_port()?;
        let port = listener.local_addr()?.port();
        let (gate, gated) = mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopping);
        let accept = thread::spawn(move || {
            let replies = Replies::new();
            thread::scope(|scope| {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    let replies = &replies;
                    let gate = &gate;
                    scope.spawn(move || {
                        let _ = serve(stream, replies, gate);
                    });
                }
            });
        });
        Ok(Self {
            port,
            gated,
            stopping,
            accept: Some(accept),
        })
    }
}

pub(super) struct Cancellation {
    socket: TcpStream,
    closed: Receiver<Instant>,
}

impl Cancellation {
    pub(super) fn observed_disconnect(self, disconnected: Instant) -> bool {
        let limit = Duration::from_secs(1);
        let observed = self
            .closed
            .recv_timeout(limit.saturating_sub(disconnected.elapsed()))
            .is_ok_and(|closed| closed >= disconnected && closed - disconnected <= limit);
        if !observed {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
        observed
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

struct Replies {
    json: String,
    first: String,
    tail: String,
    ws: String,
}

fn response(id: &str) -> Value {
    json!({
        "id":id, "object":"response", "status":"completed", "model":"upstream-model",
        "output":[{"id":"msg_bench", "type":"message", "role":"assistant", "status":"completed",
            "content":[{"type":"output_text", "text":"ok", "annotations":[]}]}],
        "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}
    })
}

impl Replies {
    fn new() -> Self {
        let http = response(HTTP_ID);
        let first = [
            json!({"type":"response.created", "response":{
                "id":HTTP_ID, "object":"response", "status":"in_progress",
                "model":"upstream-model", "output":[]
            }}),
            json!({"type":"response.output_item.added", "output_index":0, "item":{
                "id":"msg_bench", "type":"message", "role":"assistant",
                "status":"in_progress", "content":[]
            }}),
            json!({"type":"response.content_part.added", "output_index":0, "content_index":0,
                "item_id":"msg_bench", "part":{"type":"output_text", "text":"", "annotations":[]}}),
            json!({"type":"response.output_text.delta", "output_index":0, "content_index":0,
                "item_id":"msg_bench", "delta":"ok"}),
        ];
        Self {
            json: http.to_string(),
            first: first
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect(),
            tail: format!(
                "data: {}\n\n",
                json!({"type":"response.completed", "response":http})
            ),
            ws: json!({"type":"response.completed", "response":response(WS_ID)}).to_string(),
        }
    }
}

fn serve(stream: TcpStream, replies: &Replies, gate: &Sender<Cancellation>) -> io::Result<()> {
    let mut socket = wire::socket(stream, IO_TIMEOUT)?;
    while let Some(head) = Head::read(&mut socket)? {
        let mut parts = head.line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let path = target.split('?').next().unwrap_or_default();
        let length = head
            .header("content-length")
            .unwrap_or("0")
            .parse()
            .map_err(io::Error::other)?;
        let mut body = vec![0; length];
        socket.read_exact(&mut body)?;
        let stream = socket.get_mut();
        match (method, path) {
            ("GET", "/v1/responses") => return serve_websocket(socket, &head, &replies.ws),
            ("GET", "/v1/models") => {
                reply(
                    stream,
                    "200 OK",
                    "application/json",
                    &[r#"{"object":"list","data":[]}"#],
                )?;
            }
            ("POST", "/gated/v1/responses") => {
                let (close, closed) = mpsc::channel();
                gate.send(Cancellation {
                    socket: stream.try_clone()?,
                    closed,
                })
                .map_err(|_| {
                    io::Error::other("benchmark stopped waiting for the gated SSE request")
                })?;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    replies.first.len() + replies.tail.len(),
                    replies.first
                )?;
                let eof = match socket.read(&mut [0]) {
                    Ok(0) => true,
                    Err(error) if error.kind() == io::ErrorKind::ConnectionReset => true,
                    _ => false,
                };
                if eof {
                    let _ = close.send(Instant::now());
                }
                return Ok(());
            }
            ("POST", "/v1/responses") => {
                let body: Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
                if body["stream"] == true {
                    reply(
                        stream,
                        "200 OK",
                        "text/event-stream",
                        &[&replies.first, &replies.tail],
                    )?;
                } else {
                    reply(stream, "200 OK", "application/json", &[&replies.json])?;
                }
            }
            _ => {
                reply(stream, "404 Not Found", "application/json", &["{}"])?;
            }
        }
        if head
            .header("connection")
            .is_some_and(|value| value.eq_ignore_ascii_case("close"))
        {
            break;
        }
    }
    Ok(())
}

fn reply(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    parts: &[&str],
) -> io::Result<()> {
    let length: usize = parts.iter().map(|part| part.len()).sum();
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\n\r\n"
    );
    for part in parts {
        response.push_str(part);
    }
    stream.write_all(response.as_bytes())
}

fn serve_websocket(mut socket: Socket, head: &Head, completed: &str) -> io::Result<()> {
    let key = head
        .header("sec-websocket-key")
        .ok_or(io::Error::other("missing WebSocket key"))?;
    let accept = wire::websocket_accept(key);
    write!(
        socket.get_mut(),
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )?;
    socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(60)))?;
    loop {
        let (opcode, payload) = wire::read_frame(&mut socket, true)?;
        match opcode {
            1 => {
                let turn: Value = serde_json::from_slice(&payload).map_err(io::Error::other)?;
                if turn["type"] != "response.create" {
                    return Err(io::Error::other("expected response.create"));
                }
                wire::write_frame(&mut socket, 1, completed.as_bytes(), false)?;
            }
            8 => return wire::write_frame(&mut socket, 8, &payload, false),
            9 => wire::write_frame(&mut socket, 10, &payload, false)?,
            10 => {}
            _ => return Err(io::Error::other("unexpected upstream WebSocket opcode")),
        }
    }
}
