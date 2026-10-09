//! Authenticated, deadline-bounded I/O for the request-local Claude relay.

use super::super::cancellation::Cancellation;
use super::ClaudeCliError;
use crate::http::response::{response, status_text};
use emp_router::PassthroughResponse;
use polling::Event;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

use std::time::{Duration, Instant};

const MAX_REQUEST_BYTES: usize = 12 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const IO_DEADLINE: Duration = Duration::from_secs(10);

pub(super) struct RelayRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) protocol_headers: BTreeMap<String, String>,
    pub(super) body: Value,
}

struct RelayIo<'a> {
    stream: &'a mut TcpStream,
    cancelled: &'a Cancellation,
    deadline: Instant,
}

impl<'a> RelayIo<'a> {
    fn new(stream: &'a mut TcpStream, cancelled: &'a Cancellation) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        // SAFETY: RelayIo borrows this live socket and deregisters it in Drop.
        if let Err(error) = unsafe { cancelled.poller.add(&*stream, Event::none(0)) } {
            let _ = stream.set_nonblocking(false);
            return Err(error);
        }
        Ok(Self {
            stream,
            cancelled,
            deadline: Instant::now() + IO_DEADLINE,
        })
    }

    fn check(&self) -> io::Result<()> {
        if self.cancelled.is_cancelled() {
            return Err(io::ErrorKind::ConnectionAborted.into());
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(())
    }

    fn ready(&self, interest: Event) -> io::Result<()> {
        self.cancelled.poller.modify(&*self.stream, interest)?;
        self.cancelled.wait(interest, Some(self.deadline))
    }
}

impl Drop for RelayIo<'_> {
    fn drop(&mut self) {
        let _ = self.cancelled.poller.delete(&*self.stream);
        let _ = self.stream.set_nonblocking(false);
    }
}

impl Read for RelayIo<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.ready(Event::readable(0))?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl Write for RelayIo<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            match self.stream.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.ready(Event::writable(0))?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

pub(super) fn accept(
    listener: &TcpListener,
    cancelled: &Cancellation,
) -> Result<TcpStream, ClaudeCliError> {
    let result = (|| {
        listener.set_nonblocking(true)?;
        // SAFETY: listener remains borrowed until the registration is deleted below.
        unsafe {
            cancelled.poller.add(listener, Event::none(0))?;
        }
        let result = (|| loop {
            if cancelled.is_cancelled() {
                return Err(io::Error::from(io::ErrorKind::ConnectionAborted));
            }
            match listener.accept() {
                Ok((stream, _)) => return Ok(stream),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let interest = Event::readable(0);
                    cancelled.poller.modify(listener, interest)?;
                    cancelled.wait(interest, None)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        })();
        let _ = cancelled.poller.delete(listener);
        result
    })();
    result.map_err(|_| {
        if cancelled.is_cancelled() {
            ClaudeCliError::Disconnected
        } else {
            ClaudeCliError::Failure("claude_cli_relay_failed")
        }
    })
}

pub(super) fn read_accepted_request(
    stream: &mut TcpStream,
    token: &str,
    cancelled: &Cancellation,
) -> Result<RelayRequest, ClaudeCliError> {
    let mut stream = RelayIo::new(stream, cancelled)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_failed"))?;
    read_request(&mut stream, token).map_err(|error| {
        if cancelled.is_cancelled() {
            ClaudeCliError::Disconnected
        } else {
            error
        }
    })
}

fn read_request(stream: &mut RelayIo<'_>, token: &str) -> Result<RelayRequest, ClaudeCliError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let separator = loop {
        if let Some(separator) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            if separator + 4 > MAX_HEADER_BYTES {
                return Err(ClaudeCliError::Failure(
                    "claude_cli_relay_headers_too_large",
                ));
            }
            break separator;
        }
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(ClaudeCliError::Failure(
                "claude_cli_relay_headers_too_large",
            ));
        }
        let read = stream
            .read(&mut buffer)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_read_failed"))?;
        if read == 0 {
            return Err(ClaudeCliError::Failure(
                "claude_cli_relay_incomplete_request",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    };
    let headers = std::str::from_utf8(&bytes[..separator])
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_invalid_headers"))?;
    let mut lines = headers.split("\r\n");
    let request_line = lines
        .next()
        .ok_or(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .filter(|method| matches!(*method, "POST" | "HEAD"))
        .map(str::to_owned)
        .ok_or(ClaudeCliError::Failure("claude_cli_relay_invalid_method"))?;
    let target = request_parts
        .next()
        .map(str::to_owned)
        .ok_or(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"))?;
    if request_parts.next() != Some("HTTP/1.1") || request_parts.next().is_some() {
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"));
    }
    let path = target
        .split_once('?')
        .map_or(target.as_str(), |(path, _)| path)
        .to_owned();
    let mut content_length = None;
    let mut authorization = None;
    let mut protocol_headers = BTreeMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_length"));
            }
            content_length = value.parse::<usize>().ok();
        } else if name.eq_ignore_ascii_case("authorization") {
            if authorization.replace(value.to_owned()).is_some() {
                return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"));
            }
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(ClaudeCliError::Failure("claude_cli_relay_chunked_request"));
        } else if name.eq_ignore_ascii_case("anthropic-beta")
            || name.eq_ignore_ascii_case("anthropic-version")
        {
            protocol_headers.insert(name.to_ascii_lowercase(), value.to_owned());
        }
    }
    let content_length = match method.as_str() {
        "POST" => content_length
            .filter(|length| *length > 0 && *length <= MAX_REQUEST_BYTES)
            .ok_or(ClaudeCliError::Failure("claude_cli_relay_invalid_length"))?,
        "HEAD" => match content_length {
            None | Some(0) => 0,
            Some(_) => return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_length")),
        },
        _ => unreachable!("relay method was validated"),
    };
    let health_preflight = method == "HEAD" && path == "/api/hello";
    if path != "/v1/messages" && !health_preflight {
        write_error(stream, 404, "not_found");
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_path"));
    }
    if !health_preflight && method != "POST" {
        write_error(stream, 405, "method_not_allowed");
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_method"));
    }
    if !health_preflight && authorization.as_deref() != Some(&format!("Bearer {token}")) {
        write_error(stream, 401, "unauthorized");
        return Err(ClaudeCliError::Failure("claude_cli_relay_auth_failed"));
    }
    let body_start = separator + 4;
    while bytes.len().saturating_sub(body_start) < content_length {
        let remaining = content_length - bytes.len().saturating_sub(body_start);
        let read = stream
            .read(&mut buffer[..remaining.min(8192)])
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_read_failed"))?;
        if read == 0 {
            return Err(ClaudeCliError::Failure(
                "claude_cli_relay_incomplete_request",
            ));
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len().saturating_sub(body_start) != content_length {
        return Err(ClaudeCliError::Failure(
            "claude_cli_relay_extra_request_bytes",
        ));
    }
    let body = if method == "HEAD" {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[body_start..])
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_invalid_json"))?
    };
    Ok(RelayRequest {
        method,
        path,
        protocol_headers,
        body,
    })
}

pub(super) fn write_upstream_response(
    stream: &mut TcpStream,
    response_body: &PassthroughResponse,
    cancelled: &Cancellation,
) -> Result<(), ClaudeCliError> {
    let content_type = if response_body
        .content_type
        .bytes()
        .all(|byte| !byte.is_ascii_control())
    {
        response_body.content_type.as_str()
    } else {
        "application/json"
    };
    let status_line = format!(
        "HTTP/1.1 {} {}",
        response_body.status,
        status_text(response_body.status)
    );
    let bytes = response(
        &status_line,
        content_type,
        &response_body.body,
        &[("Cache-Control", "no-cache")],
    );
    write_bytes(stream, &bytes, cancelled)
}

fn write_error(stream: &mut RelayIo<'_>, status: u16, code: &str) {
    let text = status_text(status);
    let body = serde_json::to_vec(
        &serde_json::json!({"error":{"type":code,"message":"local relay rejected request"}}),
    )
    .unwrap_or_else(|_| b"{}".to_vec());
    let _ = stream.write_all(&response(
        &format!("HTTP/1.1 {status} {text}"),
        "application/json",
        &body,
        &[],
    ));
}

pub(super) fn write_bytes(
    stream: &mut TcpStream,
    bytes: &[u8],
    cancelled: &Cancellation,
) -> Result<(), ClaudeCliError> {
    let result = RelayIo::new(stream, cancelled).and_then(|mut stream| stream.write_all(bytes));
    result.map_err(|_| {
        if cancelled.is_cancelled() {
            ClaudeCliError::Disconnected
        } else {
            ClaudeCliError::Failure("claude_cli_relay_write_failed")
        }
    })
}

pub(super) fn reject_request(
    stream: &mut TcpStream,
    status: u16,
    code: &str,
    cancelled: &Cancellation,
) {
    if let Ok(mut stream) = RelayIo::new(stream, cancelled) {
        write_error(&mut stream, status, code);
    }
}

#[cfg(test)]
#[path = "relay_io_tests.rs"]
mod tests;
