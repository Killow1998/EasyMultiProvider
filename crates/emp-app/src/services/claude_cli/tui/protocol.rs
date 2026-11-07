use super::super::ClaudeCliError;
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::TcpStream,
};

pub(super) fn read(
    stream: &mut TcpStream,
    token: &str,
) -> Result<(String, Value, std::collections::BTreeMap<String, String>), ClaudeCliError> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_read_failed"))?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_write_failed"))?;
    let head = crate::http::request::read_request_head(stream)
        .ok_or(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"))?;
    let request = crate::http::request::parse_request(&head.head)
        .ok_or(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"))?;
    if request.raw_path() != "/api/hello"
        && request.header("Authorization") != Some(&format!("Bearer {token}"))
    {
        return Err(ClaudeCliError::Failure("claude_cli_relay_auth_failed"));
    }
    let path = request.raw_path().to_owned();
    let length = request
        .header("Content-Length")
        .unwrap_or("0")
        .parse::<usize>()
        .ok()
        .filter(|length| *length <= emp_transport::MAX_EXPANDED_REQUEST_BYTES)
        .ok_or(ClaudeCliError::Failure("claude_cli_input_too_large"))?;
    if request.header_count("Content-Length") > 1
        || request.header_count("Authorization") > 1
        || request.header("Transfer-Encoding").is_some()
    {
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_headers"));
    }
    let headers = ["anthropic-version", "anthropic-beta"]
        .into_iter()
        .filter_map(|key| request.header(key).map(|value| (key.into(), value.into())))
        .collect();
    let mut raw = head.body_prefix;
    let received = raw.len();
    if received > length {
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_length"));
    }
    stream
        .take((length - received) as u64)
        .read_to_end(&mut raw)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_read_failed"))?;
    if raw.len() != length {
        return Err(ClaudeCliError::Failure(
            "claude_cli_relay_incomplete_request",
        ));
    }
    let body = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&raw)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_invalid_json"))?
    };
    Ok((path, body, headers))
}

pub(super) fn reply(
    stream: &mut TcpStream,
    status: u16,
    value: &Value,
) -> Result<(), ClaudeCliError> {
    stream
        .write_all(&crate::http::response::response(
            &format!(
                "HTTP/1.1 {status} {}",
                crate::http::response::status_text(status)
            ),
            "application/json",
            &serde_json::to_vec(value).expect("MCP response"),
            &[],
        ))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_write_failed"))
}
