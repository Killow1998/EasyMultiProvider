//! HTTP request parsing, decoding and admission.

use crate::app::ServerState;
use crate::http::auth::parse_session_cookie;
use emp_transport::ContentDecodeError;
use emp_transport::RequestCapacityError;
use emp_transport::RequestLimitsConfig;
use emp_transport::TransportKind;
use emp_transport::decode_content;
use serde_json::Value;
use std::io::Read;
use std::net::TcpStream;

const MAX_HEADER_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestMethod {
    Get,
    Head,
    Post,
    Delete,
}

#[derive(Clone, Copy)]
pub(crate) struct Request<'a> {
    pub(crate) method: RequestMethod,
    pub(crate) target: &'a str,
    pub(crate) headers: &'a str,
}

impl<'a> Request<'a> {
    pub(crate) fn header(&self, name: &str) -> Option<&'a str> {
        self.headers.lines().skip(1).find_map(|line| {
            let (header_name, value) = line.split_once(':')?;
            header_name
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim())
        })
    }

    pub(crate) fn raw_path(&self) -> &'a str {
        self.target
            .split_once('?')
            .map_or(self.target, |(path, _)| path)
    }

    pub(crate) fn session_cookie(&self) -> Option<String> {
        parse_session_cookie(self.header("Cookie")?)
    }
}

pub(crate) struct RequestHead {
    pub(crate) head: String,
    pub(crate) body_prefix: Vec<u8>,
}

pub(crate) fn read_request_head(stream: &mut TcpStream) -> Option<RequestHead> {
    let mut buffer = [0_u8; 1024];
    let mut request = Vec::new();
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(separator) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            if separator > MAX_HEADER_BYTES {
                return None;
            }
            let body_prefix = request.split_off(separator + 4);
            request.truncate(separator + 4);
            return Some(RequestHead {
                head: String::from_utf8(request).ok()?,
                body_prefix,
            });
        }
        if request.len() > MAX_HEADER_BYTES + 3 {
            return None;
        }
    }
    None
}

fn request_line(request: &str) -> Option<(RequestMethod, &str)> {
    let request_line = request.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = match parts.next()? {
        "GET" => RequestMethod::Get,
        "HEAD" => RequestMethod::Head,
        "POST" => RequestMethod::Post,
        "DELETE" => RequestMethod::Delete,
        _ => return None,
    };
    let target = parts.next()?;
    (parts.next()? == "HTTP/1.1" && parts.next().is_none()).then_some((method, target))
}

pub(crate) fn parse_request(request: &str) -> Option<Request<'_>> {
    let headers_end = request.find("\r\n\r\n")?;
    let (method, target) = request_line(request)?;
    Some(Request {
        method,
        target,
        headers: &request[..headers_end],
    })
}

pub(crate) fn percent_decode(raw: &str, plus_to_space: bool) -> String {
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push((high * 16 + low) as u8);
                    index += 3;
                } else {
                    decoded.push(b'%');
                    index += 1;
                }
            }
            b'+' if plus_to_space => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub(crate) fn query_values(target: &str, name: &str) -> Vec<String> {
    let Some((_, query)) = target.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(key, _)| percent_decode(key, true) == name)
        .map(|(_, value)| percent_decode(value, true))
        .collect()
}

pub(crate) enum BodyError {
    Invalid(String),
    Capacity(RequestCapacityError),
    Decode(ContentDecodeError),
}

fn body_size_error(limit: usize, decoded: bool) -> BodyError {
    BodyError::Capacity(RequestCapacityError {
        limit,
        decoded,
        reason: emp_transport::RequestCapacityReason::HardLimit,
        available_bytes: 0,
        required_memory_bytes: 0,
        memory_total_bytes: 0,
        memory_used_bytes: 0,
        memory_used_percent: None,
    })
}

pub(crate) fn read_json_body(
    stream: &mut TcpStream,
    request: Request<'_>,
    mut body: Vec<u8>,
    state: &ServerState,
) -> Result<Value, BodyError> {
    let content_type = request
        .header("Content-Type")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if !content_type.eq_ignore_ascii_case("application/json") {
        return Err(BodyError::Invalid(
            "Content-Type must be application/json".to_owned(),
        ));
    }
    let raw_length = request.header("Content-Length").unwrap_or("0");
    if raw_length.starts_with('-') {
        if raw_length.parse::<i128>().is_ok_and(|value| value < 0) {
            return Err(BodyError::Invalid(
                "Content-Length cannot be negative".to_owned(),
            ));
        }
        return Err(BodyError::Invalid("invalid Content-Length".to_owned()));
    }
    let length = raw_length
        .parse::<u128>()
        .map_err(|_| BodyError::Invalid("invalid Content-Length".to_owned()))?;
    // Python attaches a dynamic request budget only to model-generation paths.
    // Management requests retain their fixed wire and decoded-body ceiling.
    let management = request.raw_path().starts_with("/api/");
    let limit = if management {
        5 * 1024 * 1024
    } else {
        RequestLimitsConfig::default().maximum
    };
    let length = usize::try_from(length).map_err(|_| body_size_error(limit, false))?;
    let mut budget = (!management).then(|| {
        state
            .backend
            .transport
            .request_limits
            .request(TransportKind::Http)
    });
    if let Some(budget) = &mut budget {
        budget.ensure(length).map_err(BodyError::Capacity)?;
    } else if length > limit {
        return Err(body_size_error(limit, false));
    }
    if body.len() > length {
        body.truncate(length);
    }
    while body.len() < length {
        let remaining = length - body.len();
        let mut chunk = [0_u8; 64 * 1024];
        let read_length = remaining.min(chunk.len());
        let count = stream
            .read(&mut chunk[..read_length])
            .map_err(|_| BodyError::Invalid("request body is incomplete".to_owned()))?;
        if count == 0 {
            return Err(BodyError::Invalid("request body is incomplete".to_owned()));
        }
        body.extend_from_slice(&chunk[..count]);
    }
    let body = decode_content(
        body,
        request.header("Content-Encoding").unwrap_or_default(),
        limit,
        budget.as_mut(),
    )
    .map_err(|error| match error {
        ContentDecodeError::DecodedTooLarge { limit } => body_size_error(limit, true),
        error => BodyError::Decode(error),
    })?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|error| BodyError::Invalid(format!("request body must be valid JSON: {error}")))?;
    if !value.is_object() {
        return Err(BodyError::Invalid(
            "request body must be a JSON object".to_owned(),
        ));
    }
    Ok(value)
}
