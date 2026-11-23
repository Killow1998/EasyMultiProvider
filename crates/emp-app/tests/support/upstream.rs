use std::collections::BTreeMap;
use std::io::Write;
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
    /// Parsed JSON body; `None` for empty or content-encoded (compressed) bodies.
    pub body: Option<Value>,
}

#[derive(Clone)]
struct Reply {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

/// Loopback provider that answers every request with the configured reply.
pub struct Upstream {
    port: u16,
    reply: Arc<Mutex<Reply>>,
    requests: Receiver<UpstreamRequest>,
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
        }));
        let (sender, requests) = mpsc::channel();
        let shared = Arc::clone(&reply);
        // Detached: the accept loop ends with the test process.
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let reply = shared.lock().expect("reply").clone();
                let sender = sender.clone();
                thread::spawn(move || serve_upstream(stream, &reply, &sender));
            }
        });
        Self {
            port,
            reply,
            requests,
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
        };
    }

    pub fn reply_json(&self, value: &Value) {
        self.configure(200, "application/json", value.to_string());
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

    /// No further request arrived (e.g. no silent retry after a failure).
    pub fn assert_idle(&self) {
        match self.requests.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(request) => panic!("unexpected upstream request: {request:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("upstream stopped"),
        }
    }
}

fn serve_upstream(mut stream: TcpStream, reply: &Reply, sender: &Sender<UpstreamRequest>) {
    let (request_line, headers, body) = read_message(&mut stream, false);
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let body = (!body.is_empty() && !headers.contains_key("content-encoding"))
        .then(|| serde_json::from_slice(&body).expect("JSON request"));
    let _ = sender.send(UpstreamRequest {
        method,
        path,
        headers,
        body,
    });
    let mut head = format!(
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
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
}
