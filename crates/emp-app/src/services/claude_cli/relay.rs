use crate::app::ServerState;
use crate::http::response::{response, status_text};
use crate::services::claude_cli::ClaudeCliError;
use emp_core::ResolvedRoute;
use emp_router::{ExternalRouter, PassthroughResponse};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 12 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const CHILD_POLL: Duration = Duration::from_millis(25);
const MAX_RELAY_PROBE_REQUESTS: usize = 1;

pub(super) struct RelayResult {
    pub(super) status: u16,
}

struct RelayRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    protocol_headers: BTreeMap<String, String>,
    body: Value,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_once(
    listener: TcpListener,
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    transcript: &[u8],
    token: &str,
    cancelled: &AtomicBool,
    result_tx: mpsc::SyncSender<Result<RelayResult, ClaudeCliError>>,
) {
    let mut probe_requests = 0;
    let result = loop {
        let accepted = loop {
            if cancelled.load(Ordering::Acquire) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => break Ok(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(CHILD_POLL);
                }
                Err(_) => {
                    break Err(ClaudeCliError::Failure("claude_cli_relay_failed"));
                }
            }
        };
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(error) => break Err(error),
        };
        match handle_request(
            accepted, state, route, incoming, transcript, token, cancelled,
        ) {
            Ok(Some(result)) => break Ok(result),
            Ok(None) if probe_requests < MAX_RELAY_PROBE_REQUESTS => {
                probe_requests += 1;
            }
            Ok(None) => break Err(ClaudeCliError::Failure("claude_cli_relay_probe_limit")),
            Err(error) => break Err(error),
        }
    };
    if result.is_err() {
        // A terminal provider or local relay failure must stop the CLI's
        // retry loop now; its HTTP socket has no useful response to wait for.
        cancelled.store(true, Ordering::Release);
    }
    let _ = result_tx.send(result);
}

fn handle_request(
    mut stream: TcpStream,
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    transcript: &[u8],
    token: &str,
    cancelled: &AtomicBool,
) -> Result<Option<RelayResult>, ClaudeCliError> {
    let request = read_accepted_request(&mut stream)?;
    let health_preflight = request.method == "HEAD" && request.path == "/api/hello";
    if request.path != "/v1/messages" && !health_preflight {
        write_error(&mut stream, 404, "not_found");
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_path"));
    }
    if !health_preflight && request.method != "POST" {
        write_error(&mut stream, 405, "method_not_allowed");
        return Err(ClaudeCliError::Failure("claude_cli_relay_invalid_method"));
    }
    if !health_preflight && request.authorization.as_deref() != Some(&format!("Bearer {token}")) {
        write_error(&mut stream, 401, "unauthorized");
        return Err(ClaudeCliError::Failure("claude_cli_relay_auth_failed"));
    }
    if health_preflight {
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_write_failed"))?;
        return Ok(None);
    }
    if !messages_match(&request.body, transcript) {
        write_error(&mut stream, 400, "invalid_request");
        return Err(ClaudeCliError::Failure("claude_cli_transcript_mismatch"));
    }
    if !has_only_structured_output_carrier(&request.body) {
        write_error(&mut stream, 400, "invalid_request");
        return Err(ClaudeCliError::Failure("claude_cli_tools_not_disabled"));
    }

    let router = ExternalRouter::new(&state.backend.transport.client);
    let request_body = &request.body;
    let protocol_headers = &request.protocol_headers;
    let routed = state.backend.transport.runtime.block_on(async {
        tokio::select! {
            biased;
            _ = async {
                while !cancelled.load(Ordering::Acquire) {
                    tokio::time::sleep(CHILD_POLL).await;
                }
            } => None,
            result = router.execute_anthropic_passthrough(
                route,
                request_body,
                incoming,
                protocol_headers,
            ) => Some(result),
        }
    });
    let Some(routed) = routed else {
        return Err(ClaudeCliError::Disconnected);
    };
    let routed = routed.map_err(ClaudeCliError::Router)?;
    write_upstream_response(&mut stream, &routed)?;
    Ok(Some(RelayResult {
        status: routed.status,
    }))
}

fn read_accepted_request(stream: &mut TcpStream) -> Result<RelayRequest, ClaudeCliError> {
    stream
        .set_nonblocking(false)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_failed"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_failed"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_failed"))?;
    read_request(stream)
}

fn read_request(stream: &mut TcpStream) -> Result<RelayRequest, ClaudeCliError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let separator = loop {
        if let Some(separator) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
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
            authorization = Some(value.to_owned());
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
        authorization,
        protocol_headers,
        body,
    })
}

fn messages_match(body: &Value, transcript: &[u8]) -> bool {
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let users = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .collect::<Vec<_>>();
    if users.len() != 1 {
        return false;
    }
    let content = &users[0]["content"];
    let text = match content {
        Value::String(text) => Some(text.as_str()),
        Value::Array(parts) if parts.len() == 1 => parts[0]
            .get("text")
            .and_then(Value::as_str)
            .filter(|_| parts[0].get("type").and_then(Value::as_str) == Some("text")),
        _ => None,
    };
    text.is_some_and(|text| text.as_bytes() == transcript)
}

fn has_only_structured_output_carrier(body: &Value) -> bool {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return false;
    };
    tools.len() == 1 && tools[0].get("name").and_then(Value::as_str) == Some("StructuredOutput")
}

fn write_upstream_response(
    stream: &mut TcpStream,
    response_body: &PassthroughResponse,
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
    stream
        .write_all(&response(
            &status_line,
            content_type,
            &response_body.body,
            &[("Cache-Control", "no-cache")],
        ))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_write_failed"))
}

fn write_error(stream: &mut TcpStream, status: u16, code: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn relay_accepts_only_the_exact_single_transcript_and_structured_carrier() {
        let transcript = br#"{"model":"m","input":"hello","stream":false}"#;
        let body = serde_json::json!({
            "stream":true,
            "messages":[{"role":"user","content":[{"type":"text","text":"{\"model\":\"m\",\"input\":\"hello\",\"stream\":false}"}]}],
            "tools":[{"name":"StructuredOutput","input_schema":{"type":"object"}}]
        });
        assert!(messages_match(&body, transcript));
        assert!(has_only_structured_output_carrier(&body));
        let mut changed = body.clone();
        changed["messages"][0]["content"][0]["text"] = "different".into();
        assert!(!messages_match(&changed, transcript));
        changed = body.clone();
        changed["tools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name":"Bash"}));
        assert!(!has_only_structured_output_carrier(&changed));
    }

    #[test]
    fn relay_reads_a_fragmented_request_from_a_nonblocking_accepted_socket() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay test");
        let address = listener.local_addr().expect("relay test address");
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect relay test");
            stream
                .write_all(b"HEAD /api/hello HTTP/1.1\r\n")
                .expect("write first request fragment");
            thread::sleep(Duration::from_millis(40));
            stream
                .write_all(b"Host: localhost\r\n\r\n")
                .expect("write remaining request fragment");
        });

        let (mut accepted, _) = listener.accept().expect("accept relay test");
        accepted
            .set_nonblocking(true)
            .expect("make accepted relay socket nonblocking");
        let request = read_accepted_request(&mut accepted).expect("read fragmented relay request");

        assert_eq!(request.method, "HEAD");
        assert_eq!(request.path, "/api/hello");
        assert!(request.body.is_null());
        client.join().expect("fragmented relay client");
    }
}
