pub(crate) use emp_test_support::http::read_message;
use emp_test_support::http::{Body, parse_head};
use emp_test_support::{Deadline, DeadlineIo, sse, test_timeout};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
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
    body: BufReader<Body<BufReader<DeadlineIo<TcpStream>>>>,
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
                    body: BufReader::new(Body::new(input, chunked)),
                },
                "SSE response head",
            )
            .unwrap()
    }

    #[allow(dead_code, reason = "Used by the standalone HTTP and Linux Aero tests")]
    pub fn expect_eof(&mut self, deadline: &Deadline) -> io::Result<()> {
        self.body
            .get_mut()
            .get_mut()
            .get_mut()
            .set_deadline(deadline);
        if !self.body.fill_buf()?.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "data after SSE end",
            ));
        }
        deadline.accept((), "SSE body EOF")
    }

    pub fn next_event(&mut self, deadline: &Deadline) -> io::Result<Option<Value>> {
        self.body
            .get_mut()
            .get_mut()
            .get_mut()
            .set_deadline(deadline);
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
