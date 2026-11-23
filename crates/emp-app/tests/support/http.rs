use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::IO_TIMEOUT;

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
pub(super) fn read_message(
    stream: &mut impl Read,
    until_eof: bool,
) -> (String, BTreeMap<String, String>, Vec<u8>) {
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
    stream.set_read_timeout(Some(IO_TIMEOUT)).expect("timeout");
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

pub struct SseClient {
    input: BufReader<TcpStream>,
    chunk_remaining: Option<usize>,
    ended: bool,
}

impl SseClient {
    pub fn responses(port: u16, body: &Value) -> Self {
        let body = body.to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect SSE");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("SSE timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("SSE write timeout");
        write!(
            stream,
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            super::NATIVE_TOKEN,
            body.len()
        )
        .expect("write SSE request");
        let mut input = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(input.read_line(&mut line).expect("SSE head"), 0);
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let (status, headers) = parse_head(&head);
        assert!(status.starts_with("HTTP/1.1 200 "), "{head}");
        assert!(
            headers["content-type"].contains("text/event-stream"),
            "{head}"
        );
        let chunked = headers
            .get("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
        Self {
            input,
            chunk_remaining: chunked.then_some(0),
            ended: false,
        }
    }

    fn byte(&mut self, deadline: Instant) -> io::Result<Option<u8>> {
        if self.ended {
            return Ok(None);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "SSE deadline"));
        }
        self.input.get_ref().set_read_timeout(Some(remaining))?;
        if self.chunk_remaining == Some(0) {
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            let length = usize::from_str_radix(line.split(';').next().unwrap().trim(), 16)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if length == 0 {
                self.ended = true;
                return Ok(None);
            }
            self.chunk_remaining = Some(length);
        }
        let mut byte = [0_u8; 1];
        if self.input.read(&mut byte)? == 0 {
            self.ended = true;
            return if self.chunk_remaining.is_some() {
                Err(io::Error::from(io::ErrorKind::UnexpectedEof))
            } else {
                Ok(None)
            };
        }
        if let Some(remaining) = &mut self.chunk_remaining {
            *remaining -= 1;
            if *remaining == 0 {
                let mut end = [0_u8; 2];
                self.input.read_exact(&mut end)?;
                if end != *b"\r\n" {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk ending"));
                }
            }
        }
        Ok(Some(byte[0]))
    }

    pub fn next_event(&mut self, deadline: Instant) -> io::Result<Option<Value>> {
        let mut data = String::new();
        loop {
            let mut line = Vec::new();
            loop {
                match self.byte(deadline)? {
                    Some(b'\n') => break,
                    Some(byte) => line.push(byte),
                    None if line.is_empty() && data.is_empty() => return Ok(None),
                    None => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                }
            }
            let line = String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let line = line.trim_end_matches('\r');
            if line.is_empty() && !data.is_empty() {
                if data.trim() == "[DONE]" {
                    return Ok(None);
                }
                return serde_json::from_str(&data)
                    .map(Some)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
            }
            if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
    }
}
