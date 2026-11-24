use emp_test_support::{Deadline, DeadlineIo, test_timeout};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

use super::{io_timeout, sse};

pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Response {
    #[cfg(target_os = "linux")]
    pub fn read_from(stream: TcpStream) -> Self {
        Self::read_before(stream, &Deadline::after(io_timeout()))
    }

    fn read_before(stream: TcpStream, deadline: &Deadline) -> Self {
        let (status_line, headers, body) = read_message(&mut deadline.io(stream), true);
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("status line: {status_line:?}"));
        deadline
            .accept(
                Self {
                    status,
                    headers,
                    body,
                },
                "HTTP response",
            )
            .unwrap()
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
        let size_text = std::str::from_utf8(&raw[..line_end]).expect("chunk size");
        let size = usize::from_str_radix(size_text.split(';').next().unwrap().trim(), 16)
            .expect("hex chunk size");
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
        if raw.len() < start + size + 2 {
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
pub(crate) fn read_message(
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
                if let Some(body) = dechunk(rest).expect("valid HTTP chunk trailers") {
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
            assert!(
                !headers.contains_key("content-length"),
                "HTTP body ended before Content-Length bytes"
            );
            let body = if headers
                .get("transfer-encoding")
                .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
            {
                dechunk(&rest)
                    .expect("valid HTTP chunk trailers")
                    .expect("complete chunked body")
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
    let stream = deadline
        .connect(([127, 0, 0, 1], port).into())
        .expect("connect to EMP");
    let mut io = deadline.io(&stream);
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    io.write_all(request.as_bytes()).expect("write request");
    io.write_all(body).expect("write body");
    Response::read_before(stream, deadline)
}

pub struct SseClient {
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

    fn read_head(stream: TcpStream, deadline: &Deadline) -> Self {
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
        let (status, headers) = parse_head(&head);
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
                    input,
                    chunk_remaining: chunked.then_some(0),
                    ended: false,
                },
                "SSE response head",
            )
            .unwrap()
    }

    fn read_chunk_line(&mut self, line: &mut Vec<u8>) -> io::Result<()> {
        line.clear();
        if self.input.read_until(b'\n', line)? == 0 || !line.ends_with(b"\r\n") {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        Ok(())
    }

    fn byte(&mut self) -> io::Result<Option<u8>> {
        if self.ended {
            return Ok(None);
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

    pub fn expect_eof(&mut self, deadline: &Deadline) -> io::Result<()> {
        self.input.get_mut().set_deadline(deadline);
        if self.byte()?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data after SSE end",
            ));
        }
        deadline.accept((), "SSE body EOF")
    }

    pub fn next_event(&mut self, deadline: &Deadline) -> io::Result<Option<Value>> {
        self.input.get_mut().set_deadline(deadline);
        let mut frame = String::new();
        loop {
            let mut line = Vec::new();
            loop {
                match self.byte()? {
                    Some(b'\n') => break,
                    Some(byte) => line.push(byte),
                    None if line.is_empty() && frame.is_empty() => {
                        return deadline.accept(None, "SSE body EOF");
                    }
                    None => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                }
            }
            let line = String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if line.trim_end_matches('\r').is_empty() {
                if let Some(data) = sse::data(&frame).filter(|data| !data.is_empty()) {
                    if data.trim() == "[DONE]" {
                        return deadline.accept(None, "SSE done marker");
                    }
                    let event = serde_json::from_str(&data)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    return deadline.accept(Some(event), "SSE event");
                }
                frame.clear();
            } else {
                frame.push_str(&line);
                frame.push('\n');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn both_chunk_readers_reject_invalid_trailer_fields() {
        let body = b"0\r\ngarbage\r\n\r\n";
        assert_eq!(
            dechunk(body).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        server.write_all(body).unwrap();

        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        let mut reader = SseClient::read_head(client, &deadline);
        assert_eq!(
            reader.expect_eof(&deadline).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn chunked_sse_requires_the_final_empty_line_even_after_completion() {
        for complete in [true, false] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            let event = b"data: {\"type\":\"response.completed\"}\n\n";
            write!(
                server,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                event.len()
            )
            .unwrap();
            server.write_all(event).unwrap();
            server
                .write_all(b"\r\n0\r\nX-Fixture: trailer\r\n")
                .unwrap();
            if complete {
                server.write_all(b"\r\n").unwrap();
            }

            let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
            let mut client = SseClient::read_head(client, &deadline);
            assert_eq!(
                client.next_event(&deadline).unwrap().unwrap()["type"],
                "response.completed"
            );
            let deadline = if complete {
                deadline
            } else {
                Deadline::after(test_timeout(Duration::from_millis(100)))
            };
            let eof = client.expect_eof(&deadline);
            if complete {
                eof.unwrap();
            } else {
                assert_eq!(eof.unwrap_err().kind(), io::ErrorKind::TimedOut);
            }
            drop(server);
        }
    }
}
