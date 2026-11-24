use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;

use super::{IO_TIMEOUT, http::read_message};

#[derive(Debug)]
pub struct UpstreamRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Value>,
}

#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
    gate: Option<Arc<GatedReply>>,
}

struct GatedReply {
    tail: Option<Vec<u8>>,
    released: Mutex<Receiver<()>>,
    closed: Sender<()>,
}

pub struct StreamGate {
    release: Sender<()>,
    closed: Receiver<()>,
}

impl StreamGate {
    pub fn finish(&self) {
        self.release.send(()).expect("release upstream tail");
    }

    pub fn assert_closed(&self) {
        self.closed
            .recv_timeout(Duration::from_secs(5))
            .expect("EMP must close the upstream while its tail is still gated");
    }
}

impl UpstreamRequest {
    fn is_catalog_refresh(&self) -> bool {
        self.method == "GET" && self.path.split('?').next() == Some("/v1/models")
    }
}

/// Loopback provider that answers every request with the configured reply.
pub struct Upstream {
    port: u16,
    reply: Arc<Mutex<Reply>>,
    requests: Receiver<UpstreamRequest>,
    script: Arc<Mutex<VecDeque<Reply>>>,
}

impl Upstream {
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind upstream");
        let port = listener.local_addr().expect("upstream address").port();
        let reply = Arc::new(Mutex::new(Reply {
            status: 200,
            content_type: "application/json".into(),
            body: b"{}".to_vec(),
            headers: Vec::new(),
            gate: None,
        }));
        let (sender, requests) = mpsc::channel();
        let shared = Arc::clone(&reply);
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let scripted = Arc::clone(&script);
        // Detached: the accept loop ends with the test process.
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let reply = shared.lock().expect("reply").clone();
                let sender = sender.clone();
                let script = Arc::clone(&scripted);
                thread::spawn(move || serve_upstream(stream, &reply, &sender, &script));
            }
        });
        Self {
            port,
            reply,
            requests,
            script,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn configure(&self, status: u16, content_type: &str, body: impl Into<Vec<u8>>) {
        self.configure_with_headers(status, content_type, body, &[]);
    }

    pub fn configure_with_headers(
        &self,
        status: u16,
        content_type: &str,
        body: impl Into<Vec<u8>>,
        headers: &[(&str, &str)],
    ) {
        assert!(
            matches!(self.requests.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "unconsumed upstream request"
        );
        *self.reply.lock().expect("reply") = Reply {
            status,
            content_type: content_type.into(),
            body: body.into(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
            gate: None,
        };
    }

    pub fn gate_stream(&self, first: String, tail: Option<String>) -> StreamGate {
        self.configure(200, "text/event-stream", first);
        let (release, released) = mpsc::channel();
        let (closed_sender, closed) = mpsc::channel();
        self.reply.lock().expect("reply").gate = Some(Arc::new(GatedReply {
            tail: tail.map(String::into_bytes),
            released: Mutex::new(released),
            closed: closed_sender,
        }));
        StreamGate { release, closed }
    }

    pub fn reply_json(&self, value: &Value) {
        self.configure(200, "application/json", value.to_string());
    }

    pub fn reply_response_script(&self, responses: &[Value]) {
        *self.script.lock().expect("upstream script") = responses
            .iter()
            .map(|response| Reply {
                status: 200,
                content_type: "text/event-stream".into(),
                body: format!(
                    "data: {}\n\n",
                    serde_json::json!({"type":"response.completed", "response":response})
                )
                .into_bytes(),
                headers: Vec::new(),
                gate: None,
            })
            .collect();
    }

    /// Chat-completions SSE wire: one frame per chunk, then `[DONE]`.
    pub fn reply_chat_stream(&self, chunks: &[Value]) {
        let mut wire = String::new();
        for chunk in chunks {
            wire.push_str(&format!("data: {chunk}\n\n"));
        }
        wire.push_str("data: [DONE]\n\n");
        self.configure(200, "text/event-stream", wire);
    }

    pub fn next_request(&self) -> UpstreamRequest {
        self.requests
            .recv_timeout(IO_TIMEOUT)
            .expect("EMP forwarded a request upstream")
    }

    /// Next request other than EMP's background native catalog refresh.
    #[allow(
        dead_code,
        reason = "Only forwarding and native journey targets filter catalog refreshes"
    )]
    pub fn next_forwarded(&self) -> UpstreamRequest {
        loop {
            let request = self.next_request();
            if !request.is_catalog_refresh() {
                return request;
            }
        }
    }

    /// No further upstream request arrived (e.g. no silent retry after a failure).
    pub fn assert_idle(&self) {
        match self.requests.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(request) => panic!("unexpected upstream request: {request:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("upstream stopped"),
        }
    }
}

pub(super) fn read_request(stream: &mut impl Read) -> UpstreamRequest {
    let (request_line, headers, mut body) = read_message(stream, false);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    if let Some(encoding) = headers.get("content-encoding") {
        assert_eq!(encoding, "zstd", "unsupported fixture request encoding");
        body = zstd::stream::decode_all(body.as_slice()).expect("decode upstream request");
    }
    let form = headers
        .get("content-type")
        .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"));
    let body =
        (!body.is_empty() && !form).then(|| serde_json::from_slice(&body).expect("JSON request"));
    UpstreamRequest {
        method,
        path,
        headers,
        body,
    }
}

fn serve_upstream(
    mut stream: TcpStream,
    reply: &Reply,
    sender: &Sender<UpstreamRequest>,
    script: &Mutex<VecDeque<Reply>>,
) {
    stream.set_read_timeout(Some(IO_TIMEOUT)).expect("timeout");
    let request = read_request(&mut stream);
    let scripted = if request.is_catalog_refresh() {
        None
    } else {
        script.lock().expect("upstream script").pop_front()
    };
    let reply = scripted.as_ref().unwrap_or(reply);
    let _ = sender.send(request);
    let length = reply.body.len()
        + reply
            .gate
            .as_ref()
            .map_or(0, |gate| gate.tail.as_ref().map_or(1, Vec::len));
    let mut head = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status, reply.content_type, length
    );
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    // Split frames on the network; event boundaries must survive re-assembly.
    for chunk in reply.body.chunks(73) {
        if stream
            .write_all(chunk)
            .and_then(|()| stream.flush())
            .is_err()
        {
            return;
        }
    }
    if let Some(gate) = &reply.gate {
        stream
            .set_read_timeout(Some(Duration::from_millis(25)))
            .expect("upstream disconnect polling");
        let released = gate.released.lock().expect("stream gate");
        loop {
            match released.try_recv() {
                Ok(()) => {
                    if let Some(tail) = &gate.tail {
                        let _ = stream.write_all(tail).and_then(|()| stream.flush());
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            let mut byte = [0_u8; 1];
            match stream.read(&mut byte) {
                Ok(0) => {
                    let _ = gate.closed.send(());
                    return;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                    ) =>
                {
                    let _ = gate.closed.send(());
                    return;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => panic!("unexpected bytes/error on gated upstream: {result:?}"),
            }
        }
    }
}
