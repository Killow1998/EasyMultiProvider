use crate::http::request::BodyError;
use crate::server::http::error::EntryError;
use serde_json::Value;
use std::io::Write;

pub const HEALTH_JSON_BYTES: &[u8] = b"{\"status\":\"ok\"}";

pub(crate) const SECURITY_HEADERS: &[u8] = b"X-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\nX-Content-Type-Options: nosniff\r\n";

pub(crate) fn response(
    status_line: &str,
    content_type: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut response = Vec::with_capacity(body.len() + 256);
    let _ = write!(
        response,
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if content_type == "application/json" {
        response.extend_from_slice(b"Cache-Control: no-store\r\n");
    }
    response.extend_from_slice(SECURITY_HEADERS);
    for (name, value) in headers {
        if [
            "X-Frame-Options",
            "Content-Security-Policy",
            "X-Content-Type-Options",
        ]
        .iter()
        .any(|security_header| name.eq_ignore_ascii_case(security_header))
        {
            continue;
        }
        let _ = write!(response, "{name}: {value}\r\n");
    }
    response.extend_from_slice(b"Connection: close\r\n\r\n");
    response.extend_from_slice(body);
    response
}

pub(crate) fn health_response() -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        HEALTH_JSON_BYTES,
        &[],
    )
}

pub(crate) fn not_found_response() -> Vec<u8> {
    EntryError::NotFound.wire_response()
}

pub(crate) fn unauthorized_response() -> Vec<u8> {
    EntryError::ManagementSession.wire_response()
}

pub(crate) fn cross_origin_response(message: &'static str) -> Vec<u8> {
    EntryError::CrossOrigin(message).wire_response()
}

pub(crate) fn bad_request_response() -> Vec<u8> {
    EntryError::InvalidHead.wire_response()
}

pub(crate) fn json_error_response(
    status: u16,
    status_text: &str,
    message: &str,
    code: Option<&str>,
    headers: &[(&str, &str)],
) -> Vec<u8> {
    response(
        &format!("HTTP/1.1 {status} {status_text}"),
        "application/json",
        &json_error_body(message, code),
        headers,
    )
}

pub(crate) fn json_error_body(message: &str, code: Option<&str>) -> Vec<u8> {
    let mut error = serde_json::Map::new();
    error.insert("origin".to_owned(), Value::String("emp".to_owned()));
    if let Some(code) = code {
        error.insert("code".to_owned(), Value::String(code.to_owned()));
    }
    error.insert("message".to_owned(), Value::String(message.to_owned()));
    serde_json::to_vec(&serde_json::json!({"error": error}))
        .expect("error response is JSON serializable")
}

pub(crate) fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        408 => "Request Timeout",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

pub(crate) fn body_error_response(error: BodyError) -> Vec<u8> {
    EntryError::from(error).wire_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use emp_transport::{MemoryStatus, RequestLimits, RequestLimitsConfig, TransportKind};

    #[test]
    fn capacity_errors_distinguish_unknown_from_zero_memory() {
        for memory in [None, Some(MemoryStatus::available(0))] {
            let limits = RequestLimits::new(
                RequestLimitsConfig {
                    baseline: 64,
                    maximum: 256,
                    growth_quantum: 64,
                    memory_headroom: 0,
                },
                move || memory,
                || 0,
                "capacity-wire",
            )
            .unwrap();
            let error = limits.request(TransportKind::Http).ensure(65).unwrap_err();
            let wire = String::from_utf8(body_error_response(BodyError::Capacity(error))).unwrap();
            assert!(wire.starts_with("HTTP/1.1 503 Service Unavailable"));
            let (_, body) = wire.split_once("\r\n\r\n").unwrap();
            let error = serde_json::from_str::<Value>(body).unwrap()["error"].clone();
            assert_eq!(error["code"], "request_capacity_unavailable");
            assert_eq!(
                error["reason"],
                if memory.is_some() {
                    "memory_limit"
                } else {
                    "memory_unavailable"
                }
            );
            assert_eq!(
                error["memory"]["available_bytes"],
                serde_json::json!(memory.map(|value| value.available))
            );
            assert!(error["memory"]["total_bytes"].is_null());
            assert!(error["memory"]["used_bytes"].is_null());
        }
    }
}
