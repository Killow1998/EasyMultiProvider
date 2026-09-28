//! Black-box fixtures for user journeys: a real EMP process started from the
//! built binary with temporary CODEX_HOME/config roots, plus an in-process
//! loopback upstream that records every request it receives.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

/// Synthetic Codex login token; `/v1/*` callers present it as a bearer token.
pub const NATIVE_TOKEN: &str = "fixture-native-token";
/// Synthetic provider API key; it must never be echoed by management APIs.
pub const PROVIDER_KEY: &str = "fixture-provider-key";
pub const ORIGINAL_CODEX_CONFIG: &str = "# user settings\n[features]\nweb_search = true\n";

const IO_TIMEOUT: Duration = Duration::from_secs(15);

// ---------------------------------------------------------------- HTTP client

pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("JSON body ({error}): {}", self.text()))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Parsed `data:` frames of a server-sent event stream.
    pub fn sse_events(&self) -> Vec<Value> {
        self.text()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).expect("SSE data JSON"))
            .collect()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_head(head: &str) -> (String, BTreeMap<String, String>) {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    (first, headers)
}

/// Decode a complete chunked body, or `None` while more bytes are needed.
fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line_end = find(raw, b"\r\n")?;
        let size_text = std::str::from_utf8(&raw[..line_end]).expect("chunk size");
        let size =
            usize::from_str_radix(size_text.split(';').next()?.trim(), 16).expect("hex chunk size");
        let start = line_end + 2;
        if size == 0 {
            return raw[start..].starts_with(b"\r\n").then_some(body);
        }
        if raw.len() < start + size + 2 {
            return None;
        }
        body.extend_from_slice(&raw[start..start + size]);
        raw = &raw[start + size + 2..];
    }
}

fn sse_finished(body: &[u8]) -> bool {
    String::from_utf8_lossy(body).split("\n\n").any(|frame| {
        frame.lines().any(|line| {
            line.strip_prefix("data: ")
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                .and_then(|event| event["type"].as_str().map(str::to_owned))
                .is_some_and(|kind| {
                    matches!(
                        kind.as_str(),
                        "response.completed" | "response.incomplete" | "response.failed"
                    )
                })
        })
    }) && body.ends_with(b"\n\n")
}

/// Read one HTTP message (request or response) whose head ends at `\r\n\r\n`.
/// Returns the first line, lowercase headers and the decoded body.
fn read_message(
    stream: &mut TcpStream,
    until_eof: bool,
) -> (String, BTreeMap<String, String>, Vec<u8>) {
    stream.set_read_timeout(Some(IO_TIMEOUT)).expect("timeout");
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        if let Some(separator) = find(&raw, b"\r\n\r\n") {
            let head = std::str::from_utf8(&raw[..separator]).expect("ASCII head");
            let (first, headers) = parse_head(head);
            let rest = &raw[separator + 4..];
            if let Some(length) = headers.get("content-length") {
                let length: usize = length.parse().expect("Content-Length");
                if rest.len() >= length {
                    return (first, headers, rest[..length].to_vec());
                }
            } else if headers
                .get("transfer-encoding")
                .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
            {
                if let Some(body) = dechunk(rest) {
                    return (first, headers, body);
                }
            } else if !until_eof {
                return (first, headers, Vec::new());
            } else if headers
                .get("content-type")
                .is_some_and(|value| value.contains("text/event-stream"))
                && sse_finished(rest)
            {
                return (first, headers, rest.to_vec());
            }
        }
        let count = stream.read(&mut buffer).expect("read HTTP message");
        if count == 0 {
            let separator = find(&raw, b"\r\n\r\n").expect("complete HTTP head before EOF");
            let head = std::str::from_utf8(&raw[..separator]).expect("ASCII head");
            let (first, headers) = parse_head(head);
            let rest = raw[separator + 4..].to_vec();
            let body = if headers
                .get("transfer-encoding")
                .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
            {
                dechunk(&rest).expect("complete chunked body")
            } else {
                rest
            };
            return (first, headers, body);
        }
        raw.extend_from_slice(&buffer[..count]);
    }
}

pub fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to EMP");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    stream.write_all(request.as_bytes()).expect("write request");
    stream.write_all(body).expect("write body");
    let (status_line, headers, body) = read_message(&mut stream, true);
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("status line: {status_line:?}"));
    Response {
        status,
        headers,
        body,
    }
}

// ----------------------------------------------------------- fake upstream

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

// ---------------------------------------------------------------- EMP process

pub fn emp_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_EMP"));
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "EASY_MULTI_PROVIDER_CONFIG",
        "EASY_MULTI_PROVIDER_MASTER_KEY_FILE",
        "BROWSER",
    ] {
        command.env_remove(key);
    }
    // Any accidental real-network access fails fast against a closed port.
    command
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("TZ", "UTC");
    command
}

/// A temporary user: CODEX_HOME with a synthetic login and user TOML, plus
/// an EMP configuration directory.
pub struct Workspace {
    _directory: TempDir,
    pub root: PathBuf,
    pub codex_home: PathBuf,
    pub config_path: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        // Short /tmp paths keep nested AF_UNIX control sockets within limits.
        let directory = tempfile::Builder::new()
            .prefix("emp-j-")
            .tempdir_in(std::env::temp_dir())
            .expect("temporary root");
        let root = directory.path().canonicalize().expect("canonical root");
        let codex_home = root.join("codex");
        std::fs::create_dir_all(codex_home.join("sessions")).expect("CODEX_HOME");
        std::fs::create_dir_all(root.join("home")).expect("HOME");
        std::fs::write(
            codex_home.join("auth.json"),
            json!({"tokens": {"access_token": NATIVE_TOKEN, "account_id": "fixture-account"}})
                .to_string(),
        )
        .expect("auth.json");
        std::fs::write(codex_home.join("config.toml"), ORIGINAL_CODEX_CONFIG).expect("TOML");
        std::fs::write(root.join("native.json"), r#"{"models":[]}"#).expect("native catalog");
        let config_path = root.join("config.json");
        Self {
            _directory: directory,
            root,
            codex_home,
            config_path,
        }
    }

    /// Four routes, one per upstream protocol, all pointing at `upstream`.
    pub fn with_routes(upstream: &Upstream) -> Self {
        let workspace = Self::new();
        let providers: Vec<Value> = [
            ("test", "chat_completions"),
            ("anthropic", "anthropic_messages"),
            ("responses", "responses"),
            ("native", "responses"),
        ]
        .iter()
        .map(|(id, protocol)| {
            if *id == "native" {
                json!({"id": id, "base_url": upstream.base_url(), "protocol": protocol,
                       "auth_mode": "forward"})
            } else {
                json!({"id": id, "base_url": upstream.base_url(), "protocol": protocol,
                       "auth_mode": "api_key", "api_key": PROVIDER_KEY})
            }
        })
        .collect();
        let models: Vec<Value> = ["test", "anthropic", "responses", "native"]
            .iter()
            .map(|id| {
                json!({"id": format!("{id}/model"), "provider": id,
                       "upstream_id": "upstream-model", "context_window": 256000,
                       "enabled": true})
            })
            .collect();
        workspace.write_config(json!({"providers": providers, "models": models}));
        workspace
    }

    /// Write an EMP configuration; `native_catalog_path` is always filled in.
    pub fn write_config(&self, mut config: Value) {
        config["native_catalog_path"] = json!(self.root.join("native.json"));
        std::fs::write(&self.config_path, config.to_string()).expect("EMP config");
    }

    pub fn codex_config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
    }

    pub fn read_codex_config(&self) -> String {
        std::fs::read_to_string(self.codex_config()).expect("read Codex TOML")
    }

    pub fn integration_dir(&self) -> PathBuf {
        self.codex_home.join("easy-multi-provider/integration")
    }

    /// Environment for any EMP process acting on this workspace.
    pub fn command(&self) -> Command {
        let mut command = emp_command();
        command
            .env("CODEX_HOME", &self.codex_home)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            // Outside the config directory, so the config counts as "custom".
            .current_dir(self.root.join("home"));
        command
    }

    pub fn serve_arguments(&self) -> Vec<String> {
        vec![
            "serve".into(),
            "--config".into(),
            self.config_path.display().to_string(),
            "--host".into(),
            "127.0.0.1".into(),
            "--port".into(),
            "0".into(),
        ]
    }

    pub fn start(&self) -> Emp {
        let mut command = self.command();
        command.args(self.serve_arguments());
        Emp::spawn(command)
    }
}

/// A running EMP service with an authenticated management session.
pub struct Emp {
    child: Child,
    pub port: u16,
    pub session: String,
    pub bootstrap: String,
    pub startup_output: Vec<String>,
}

impl Emp {
    pub fn spawn(mut command: Command) -> Self {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start EMP");
        let stdout = child.stdout.take().expect("EMP stdout");
        let stderr = child.stderr.take().expect("EMP stderr");
        let (lines, received) = mpsc::channel::<String>();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = lines.send(line);
            }
        });
        let errors = Arc::new(Mutex::new(String::new()));
        let collected = Arc::clone(&errors);
        thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            collected.lock().expect("stderr").push_str(&text);
        });
        let mut startup_output = Vec::new();
        let opened = loop {
            match received.recv_timeout(Duration::from_secs(30)) {
                Ok(line) => {
                    let opened = line.strip_prefix("Open in browser: ").map(str::to_owned);
                    startup_output.push(line);
                    if let Some(url) = opened {
                        break url;
                    }
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    thread::sleep(Duration::from_millis(50));
                    panic!(
                        "EMP not ready ({error}); stdout: {startup_output:?}; stderr: {}",
                        errors.lock().expect("stderr")
                    );
                }
            }
        };
        // Keep draining stdout so the service never blocks on a full pipe.
        thread::spawn(move || while received.recv().is_ok() {});
        let (origin, query) = opened
            .split_once("/?bootstrap=")
            .unwrap_or_else(|| panic!("bootstrap URL: {opened}"));
        let port = origin
            .strip_prefix("http://127.0.0.1:")
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("loopback URL: {opened}"));
        let bootstrap = query.to_owned();
        let exchanged = http(
            port,
            "POST",
            "/api/session",
            &[("X-EMP-Bootstrap", &bootstrap)],
            b"",
        );
        assert_eq!(exchanged.status, 200, "{}", exchanged.text());
        let session = exchanged.json()["session"]
            .as_str()
            .expect("session token")
            .to_owned();
        let emp = Self {
            child,
            port,
            session,
            bootstrap,
            startup_output,
        };
        // `--port 0` is a test-only listener choice; saving settings through
        // the page requires the real port, as a user's config always has.
        let mut config = emp.get("/api/config").json();
        if config["port"] == 0 {
            config["port"] = json!(port);
            let saved = emp.post("/api/config", &config);
            assert_eq!(saved.status, 200, "{}", saved.text());
        }
        emp
    }

    pub fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("pid")
    }

    /// Management request carrying the browser session.
    pub fn api(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let mut headers = vec![("X-EMP-Session", self.session.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        http(self.port, method, path, &headers, payload.as_bytes())
    }

    pub fn get(&self, path: &str) -> Response {
        self.api("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Response {
        self.api("POST", path, Some(body))
    }

    /// Unauthenticated request (no session, no bearer token).
    pub fn anonymous(&self, method: &str, path: &str) -> Response {
        http(self.port, method, path, &[], b"")
    }

    /// Codex-style model request: the native login token as bearer.
    pub fn codex(&self, method: &str, path: &str, body: Option<&Value>) -> Response {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let bearer = format!("Bearer {NATIVE_TOKEN}");
        let mut headers = vec![("Authorization", bearer.as_str())];
        if body.is_some() {
            headers.push(("Content-Type", "application/json"));
        }
        http(self.port, method, path, &headers, payload.as_bytes())
    }

    pub fn responses(&self, body: &Value) -> Response {
        self.codex("POST", "/v1/responses", Some(body))
    }

    pub fn signal(&self, signal: i32) {
        // SAFETY: kill(2) on our own live child process id.
        let result = unsafe { libc::kill(self.pid(), signal) };
        assert_eq!(result, 0, "signal EMP");
    }

    /// Wait for the process to exit and return its exit code.
    pub fn wait_exit(&mut self) -> Option<i32> {
        for _ in 0..800 {
            if let Some(status) = self.child.try_wait().expect("poll EMP") {
                return status.code();
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("EMP did not exit within 8 seconds");
    }

    /// Graceful stop (SIGTERM), as a user closing the service would do.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        self.signal(libc::SIGTERM);
        for _ in 0..500 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Emp {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent directory");
    }
    std::fs::write(path, contents).expect("write fixture file");
}
