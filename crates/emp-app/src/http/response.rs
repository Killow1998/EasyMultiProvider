//! Wire response framing and public HTTP errors.

use crate::http::request::BodyError;
use emp_transport::ContentDecodeError;
use emp_transport::RequestCapacityError;
use serde_json::Value;
use std::io::Write;

/// Exact compact JSON body observed in the Python server tests.
pub const HEALTH_JSON_BYTES: &[u8] = b"{\"status\":\"ok\"}";

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
    for (name, value) in headers {
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
    response(
        "HTTP/1.1 404 Not Found",
        "application/json",
        b"{\"error\":{\"message\":\"not found\"}}",
        &[],
    )
}

pub(crate) fn unauthorized_response() -> Vec<u8> {
    response(
        "HTTP/1.1 401 Unauthorized",
        "application/json",
        b"{\"error\":{\"message\":\"management session is required\"}}",
        &[],
    )
}

pub(crate) fn cross_origin_response(message: &str) -> Vec<u8> {
    response(
        "HTTP/1.1 403 Forbidden",
        "application/json",
        format!("{{\"error\":{{\"message\":\"{message}\"}}}}").as_bytes(),
        &[],
    )
}

pub(crate) fn bad_request_response() -> Vec<u8> {
    response(
        "HTTP/1.1 400 Bad Request",
        "application/json",
        b"{\"error\":{\"message\":\"invalid HTTP request\"}}",
        &[],
    )
}

pub(crate) fn json_error_response(
    status: u16,
    status_text: &str,
    message: &str,
    code: Option<&str>,
    headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut error = serde_json::Map::new();
    if let Some(code) = code {
        error.insert("code".to_owned(), Value::String(code.to_owned()));
    }
    error.insert("message".to_owned(), Value::String(message.to_owned()));
    let body = serde_json::to_vec(&serde_json::json!({"error": error}))
        .expect("error response is JSON serializable");
    response(
        &format!("HTTP/1.1 {status} {status_text}"),
        "application/json",
        &body,
        headers,
    )
}

pub(crate) fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

fn capacity_response(error: &RequestCapacityError) -> Vec<u8> {
    let capacity = error.http_status() == 503;
    let code = if capacity {
        "request_capacity_unavailable"
    } else {
        "request_too_large"
    };
    let mut detail = serde_json::json!({
        "code": code,
        "message": error.to_string(),
        "limit_bytes": error.limit,
    });
    if capacity {
        detail["memory"] = serde_json::json!({
            "used_percent": error.memory_used_percent,
            "used_bytes": error.memory_used_bytes,
            "total_bytes": error.memory_total_bytes,
            "available_bytes": error.available_bytes,
            "required_bytes": error.required_memory_bytes,
        });
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail}))
        .expect("capacity response is JSON serializable");
    let retry = capacity.then_some(("Retry-After", "2"));
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.http_status(),
            status_text(error.http_status())
        ),
        "application/json",
        &body,
        &retry.into_iter().collect::<Vec<_>>(),
    )
}

pub(crate) fn body_error_response(error: BodyError) -> Vec<u8> {
    match error {
        BodyError::Invalid(message) => {
            json_error_response(400, status_text(400), &message, None, &[])
        }
        BodyError::Capacity(error) => capacity_response(&error),
        BodyError::Decode(ContentDecodeError::Capacity(error)) => capacity_response(&error),
        BodyError::Decode(error) => json_error_response(
            error.http_status(),
            status_text(error.http_status()),
            &error.to_string(),
            (error.http_status() == 413).then_some("request_too_large"),
            &[],
        ),
    }
}
