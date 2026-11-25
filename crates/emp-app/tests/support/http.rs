use emp_test_support::{Deadline, DeadlineIo, sse, test_timeout};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

use super::io_timeout;

pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    #[cfg(target_os = "linux")]
    pub fn read_from(stream: TcpStream) -> io::Result<Self> {
        Self::read_before(stream, &Deadline::after(io_timeout()))
    }

    pub(crate) fn read_before(stream: TcpStream, deadline: &Deadline) -> io::Result<Self> {
        let (status_line, headers, body) = read_message(&mut deadline.io(stream), true)?;
        let mut parts = status_line.split_whitespace();
        let status = parts
            .next()
            .filter(|version| matches!(*version, "HTTP/1.0" | "HTTP/1.1"))
            .and_then(|_| parts.next())
            .filter(|code| code.len() == 3)
            .and_then(|code| code.parse().ok())
            .filter(|status| (100..=599).contains(status))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("status line: {status_line:?}"),
                )
            })?;
        deadline.accept(
            Self {
                status,
                headers,
                body,
            },
            "HTTP response",
        )
    }

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

    pub fn sse_events(&self) -> Vec<Value> {
        sse::frames(&self.body)
            .filter_map(|frame| frame.expect("complete SSE response frame").data())
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(&data).expect("SSE data JSON"))
            .collect()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn parse_head(head: &str) -> (String, impl Iterator<Item = (String, String)> + '_) {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    (first, headers)
}

fn trailer_is_end(line: &[u8]) -> io::Result<bool> {
    if line.is_empty() {
        return Ok(true);
    }
    let valid = line
        .iter()
        .position(|&byte| byte == b':')
        .is_some_and(|colon| {
            colon > 0
                && line[..colon]
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^`_|~".contains(byte))
                && line[colon + 1..]
                    .iter()
                    .all(|&byte| byte == b'\t' || (byte >= b' ' && byte != 127))
        });
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid chunk trailer field",
        ));
    }
    Ok(false)
}

fn dechunk(mut raw: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = find(raw, b"\r\n") else {
            return Ok(None);
        };
        let size_text = std::str::from_utf8(&raw[..line_end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let size_text = size_text
            .split_once(';')
            .map_or(size_text, |(size, _)| size);
        let size = usize::from_str_radix(size_text.trim(), 16)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let start = line_end + 2;
        if size == 0 {
            let mut trailers = &raw[start..];
            while let Some(end) = find(trailers, b"\r\n") {
                if trailer_is_end(&trailers[..end])? {
                    return Ok(Some(body));
                }
                trailers = &trailers[end + 2..];
            }
            return Ok(None);
        }
        if raw[start..].len().saturating_sub(2) < size {
            return Ok(None);
        }
        let end = start + size;
        if &raw[end..end + 2] != b"\r\n" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk ending"));
        }
        body.extend_from_slice(&raw[start..end]);
        raw = &raw[end + 2..];
    }
}

fn sse_finished(body: &[u8]) -> bool {
    if !body.ends_with(b"\n") {
        return false;
    }
    let mut finished = false;
    for frame in sse::frames(body) {
        let Ok(frame) = frame else {
            return false;
        };
        if !finished
            && let Some(data) = frame.data()
            && let Ok(event) = serde_json::from_str::<Value>(&data)
        {
            finished = matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.incomplete" | "response.failed")
            );
        }
    }
    finished
}

/// Read one HTTP message (request or response) whose head ends at `\r\n\r\n`.
/// Returns the first line, lowercase headers and the decoded body.
pub(crate) fn read_message(
    stream: &mut impl Read,
    until_eof: bool,
) -> io::Result<(String, BTreeMap<String, String>, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 8192];
    let separator = loop {
        if let Some(separator) = find(&raw, b"\r\n\r\n") {
            break separator;
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete HTTP head",
            ));
        }
        raw.extend_from_slice(&buffer[..count]);
    };
    let head = std::str::from_utf8(&raw[..separator])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let (first, fields) = parse_head(head);
    let mut headers = BTreeMap::new();
    let mut length = None;
    for (name, value) in fields {
        if name == "content-length" {
            let declared = value
                .parse::<usize>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if let Some(previous) = length.replace(declared)
                && previous != declared
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "conflicting Content-Length fields",
                ));
            }
        }
        headers.insert(name, value);
    }
    if length.is_some() && headers.contains_key("transfer-encoding") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Content-Length and Transfer-Encoding cannot be combined",
        ));
    }
    let chunked = headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    let sse = headers
        .get("content-type")
        .is_some_and(|value| value.contains("text/event-stream"));
    loop {
        let rest = &raw[separator + 4..];
        if let Some(length) = length {
            if let Some(body) = rest.get(..length) {
                return Ok((first, headers, body.to_vec()));
            }
        } else if chunked {
            if let Some(body) = dechunk(rest)? {
                return Ok((first, headers, body));
            }
        } else if !until_eof {
            return Ok((first, headers, Vec::new()));
        } else if sse && sse_finished(rest) {
            return Ok((first, headers, rest.to_vec()));
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            let incomplete = if length.is_some() {
                "HTTP body ended before Content-Length bytes"
            } else if chunked {
                "incomplete chunked body"
            } else {
                return Ok((first, headers, rest.to_vec()));
            };
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, incomplete));
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
    http_before(
        port,
        method,
        path,
        headers,
        body,
        &Deadline::after(io_timeout()),
    )
}

pub(crate) fn http_before(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    deadline: &Deadline,
) -> Response {
    let stream =
        send_request(port, method, path, headers, body, deadline).expect("send EMP request");
    Response::read_before(stream, deadline).expect("read EMP response")
}

pub(super) fn send_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    deadline: &Deadline,
) -> io::Result<TcpStream> {
    let stream = deadline.connect(([127, 0, 0, 1], port).into())?;
    let mut io = deadline.io(&stream);
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    io.write_all(request.as_bytes())?;
    io.write_all(body)?;
    Ok(stream)
}

pub struct SseClient {
    body: BufReader<SseBody>,
}

struct SseBody {
    input: BufReader<DeadlineIo<TcpStream>>,
    chunk_remaining: Option<usize>,
    ended: bool,
}

impl SseClient {
    pub fn responses(port: u16, body: &Value) -> Self {
        let body = body.to_string();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        let stream = deadline
            .connect(([127, 0, 0, 1], port).into())
            .expect("connect SSE");
        write!(
            deadline.io(&stream),
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            super::NATIVE_TOKEN,
            body.len()
        )
        .expect("write SSE request");
        Self::read_head(
            stream,
            &Deadline::after(test_timeout(Duration::from_secs(5))),
        )
    }

    pub(crate) fn read_head(stream: TcpStream, deadline: &Deadline) -> Self {
        let mut input = BufReader::new(deadline.io(stream));
        let mut head = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(input.read_line(&mut line).expect("SSE head"), 0);
            head.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let (status, fields) = parse_head(&head);
        let headers: BTreeMap<_, _> = fields.collect();
        assert!(status.starts_with("HTTP/1.1 200 "), "{head}");
        assert!(
            headers["content-type"].contains("text/event-stream"),
            "{head}"
        );
        let chunked = headers
            .get("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
        deadline
            .accept(
                Self {
                    body: BufReader::new(SseBody {
                        input,
                        chunk_remaining: chunked.then_some(0),
                        ended: false,
                    }),
                },
                "SSE response head",
            )
            .unwrap()
    }

    #[allow(dead_code, reason = "Used by the standalone HTTP and Linux Aero tests")]
    pub fn expect_eof(&mut self, deadline: &Deadline) -> io::Result<()> {
        self.body.get_mut().input.get_mut().set_deadline(deadline);
        if !self.body.fill_buf()?.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data after SSE end",
            ));
        }
        deadline.accept((), "SSE body EOF")
    }

    pub fn next_event(&mut self, deadline: &Deadline) -> io::Result<Option<Value>> {
        self.body.get_mut().input.get_mut().set_deadline(deadline);
        while let Some(frame) = sse::read_frame(&mut self.body)? {
            if let Some(data) = frame.data().filter(|data| !data.is_empty()) {
                if data.trim() == "[DONE]" {
                    return deadline.accept(None, "SSE done marker");
                }
                let event = serde_json::from_str(&data)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                return deadline.accept(Some(event), "SSE event");
            }
        }
        deadline.accept(None, "SSE body EOF")
    }
}

impl SseBody {
    fn read_chunk_line(&mut self, line: &mut Vec<u8>) -> io::Result<()> {
        line.clear();
        if self.input.read_until(b'\n', line)? == 0 || !line.ends_with(b"\r\n") {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        Ok(())
    }
}

impl Read for SseBody {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.ended || buffer.is_empty() {
            return Ok(0);
        }
        if self.chunk_remaining == Some(0) {
            let mut line = Vec::new();
            self.read_chunk_line(&mut line)?;
            let size = std::str::from_utf8(line.split(|&byte| byte == b';').next().unwrap())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let length = usize::from_str_radix(size.trim(), 16)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if length == 0 {
                loop {
                    self.read_chunk_line(&mut line)?;
                    if trailer_is_end(&line[..line.len() - 2])? {
                        break;
                    }
                }
                self.ended = true;
                return Ok(0);
            }
            self.chunk_remaining = Some(length);
        }
        let length = self
            .chunk_remaining
            .unwrap_or(buffer.len())
            .min(buffer.len());
        let count = self.input.read(&mut buffer[..length])?;
        if count == 0 {
            self.ended = true;
            return if self.chunk_remaining.is_some() {
                Err(io::Error::from(io::ErrorKind::UnexpectedEof))
            } else {
                Ok(0)
            };
        }
        if let Some(remaining) = &mut self.chunk_remaining {
            *remaining -= count;
            if *remaining == 0 {
                let mut end = [0_u8; 2];
                self.input.read_exact(&mut end)?;
                if end != *b"\r\n" {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "chunk ending"));
                }
            }
        }
        Ok(count)
    }
}
