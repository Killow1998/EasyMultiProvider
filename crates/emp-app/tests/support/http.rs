use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;

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
