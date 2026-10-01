use super::media::ExpectedUserContent;
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
    expected_user_content: &ExpectedUserContent,
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
            accepted,
            state,
            route,
            incoming,
            expected_user_content,
            token,
            cancelled,
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
    expected_user_content: &ExpectedUserContent,
    token: &str,
    cancelled: &AtomicBool,
) -> Result<Option<RelayResult>, ClaudeCliError> {
    let mut request = read_accepted_request(&mut stream)?;
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
    if !normalize_cli_system_messages(&mut request.body)
        || !messages_match(&request.body, expected_user_content)
    {
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

fn normalize_cli_system_messages(body: &mut Value) -> bool {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let mut user_message = None;
    let mut moved_system_blocks = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("user") if user_message.is_none() => user_message = Some(message.clone()),
            Some("user") | Some("assistant") => return false,
            Some("system") => {
                let Some(fields) = message.as_object() else {
                    return false;
                };
                if !fields.contains_key("content")
                    || fields
                        .keys()
                        .any(|key| !matches!(key.as_str(), "role" | "content" | "output_config"))
                {
                    return false;
                }
                // Some CLI models repeat the request's effort on their date
                // reminder. Discard only that identical duplicate; the actual
                // request setting remains at the Anthropic top level.
                if let Some(output_config) = fields.get("output_config") {
                    let Some(settings) = output_config.as_object() else {
                        return false;
                    };
                    if settings.len() != 1
                        || !settings.get("effort").is_some_and(Value::is_string)
                        || settings.get("effort")
                            != body.get("output_config").and_then(|v| v.get("effort"))
                    {
                        return false;
                    }
                }
                match message.get("content") {
                    Some(Value::String(text)) => {
                        moved_system_blocks.push(serde_json::json!({"type":"text","text":text}));
                    }
                    Some(Value::Array(blocks)) if !blocks.is_empty() => {
                        if blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) != Some("text")
                                || !block.get("text").is_some_and(Value::is_string)
                        }) {
                            return false;
                        }
                        moved_system_blocks.extend(blocks.iter().cloned());
                    }
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    let Some(user_message) = user_message else {
        return false;
    };
    let mut system_blocks = match body.get("system") {
        None => Vec::new(),
        Some(Value::String(text)) => vec![serde_json::json!({"type":"text","text":text})],
        Some(Value::Array(blocks)) => {
            if blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) != Some("text")
                    || !block.get("text").is_some_and(Value::is_string)
            }) {
                return false;
            }
            blocks.clone()
        }
        Some(_) => return false,
    };
    system_blocks.extend(moved_system_blocks);

    let Some(object) = body.as_object_mut() else {
        return false;
    };
    object.insert("messages".to_owned(), Value::Array(vec![user_message]));
    if !system_blocks.is_empty() {
        object.insert("system".to_owned(), Value::Array(system_blocks));
    }
    true
}

fn messages_match(body: &Value, expected: &ExpectedUserContent) -> bool {
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return false;
    };
    let [message] = messages.as_slice() else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match expected {
        ExpectedUserContent::Text(transcript) => {
            let content = &message["content"];
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
        ExpectedUserContent::Blocks(expected) => {
            let Some(content) = message.get("content").and_then(Value::as_array) else {
                return false;
            };
            matches_expected_blocks(content, expected)
        }
    }
}

fn matches_expected_blocks(actual: &[Value], expected: &[Value]) -> bool {
    let mut actual_index = 0;
    let mut deferred_resizes = Vec::new();
    for expected_block in expected {
        let Some(actual_block) = actual.get(actual_index) else {
            return false;
        };
        if block_matches(expected_block, actual_block) {
            actual_index += 1;
            continue;
        }
        if expected_block.get("type").and_then(Value::as_str) != Some("image")
            || actual_block.get("type").and_then(Value::as_str) != Some("image")
        {
            return false;
        }
        if let Some(note) = actual.get(actual_index + 1)
            && super::image_geometry::matches_resize(expected_block, actual_block, note)
        {
            actual_index += 2;
        } else {
            deferred_resizes.push((expected_block, actual_block));
            actual_index += 1;
        }
    }
    let Some(trailing) = actual.get(actual_index..) else {
        return false;
    };
    if trailing.len() != deferred_resizes.len() {
        return false;
    }
    deferred_resizes
        .iter()
        .zip(trailing)
        .all(|((expected_image, actual_image), note)| {
            super::image_geometry::matches_resize(expected_image, actual_image, note)
        })
}

fn block_matches(expected: &Value, actual: &Value) -> bool {
    if expected == actual {
        return true;
    }
    let (Some(expected_fields), Some(actual_fields)) = (expected.as_object(), actual.as_object())
    else {
        return false;
    };
    if expected_fields.len() != actual_fields.len()
        || expected_fields
            .iter()
            .any(|(key, value)| key != "text" && actual_fields.get(key) != Some(value))
        || expected_fields.get("type").and_then(Value::as_str) != Some("text")
    {
        return false;
    }
    let Some(expected_text) = expected_fields.get("text").and_then(Value::as_str) else {
        return false;
    };
    let Some(actual_text) = actual_fields.get("text").and_then(Value::as_str) else {
        return false;
    };
    let (Some(expected_value), Some(actual_value)) = (
        transcript_marker_value(expected_text),
        transcript_marker_value(actual_text),
    ) else {
        return false;
    };
    expected_value == actual_value
}

fn transcript_marker_value(text: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(text).ok()?;
    let object = value.as_object()?;
    let metadata_marker = object.len() == 1 && object.contains_key("codex_responses_metadata");
    let item_marker = object
        .get("codex_item_index")
        .and_then(Value::as_u64)
        .is_some()
        && object.get("item").is_some_and(Value::is_object);
    (metadata_marker || item_marker).then_some(value)
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
        assert!(messages_match(
            &body,
            &ExpectedUserContent::Text(transcript.to_vec())
        ));
        assert!(has_only_structured_output_carrier(&body));
        let mut changed = body.clone();
        changed["messages"][0]["content"][0]["text"] = "different".into();
        assert!(!messages_match(
            &changed,
            &ExpectedUserContent::Text(transcript.to_vec())
        ));
        changed = body.clone();
        changed["tools"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"name":"Bash"}));
        assert!(!has_only_structured_output_carrier(&changed));
    }

    #[test]
    fn relay_requires_exact_ordered_text_and_media_blocks() {
        let expected = vec![
            serde_json::json!({"type":"text","text":"item 0 image part 1 follows"}),
            serde_json::json!({"type":"image","source":{"type":"url","url":"https://images.invalid/a.png"}}),
        ];
        let body = serde_json::json!({
            "stream":true,
            "messages":[{"role":"user","content":expected}],
            "tools":[{"name":"StructuredOutput"}]
        });
        assert!(messages_match(
            &body,
            &ExpectedUserContent::Blocks(expected.clone())
        ));

        let mut changed = body.clone();
        changed["messages"][0]["content"][1]["source"]["url"] =
            "https://images.invalid/other.png".into();
        assert!(!messages_match(
            &changed,
            &ExpectedUserContent::Blocks(expected.clone())
        ));
        changed = body;
        changed["messages"][0]["content"]
            .as_array_mut()
            .unwrap()
            .reverse();
        assert!(!messages_match(
            &changed,
            &ExpectedUserContent::Blocks(expected)
        ));
    }

    #[test]
    fn relay_canonicalizes_only_marker_text_and_rejects_outer_metadata_changes() {
        let marker = serde_json::json!({
            "codex_item_index":1,
            "item":{
                "type":"function_call_output",
                "call_id":"call-fixture",
                "output":{"type":"thinking","reasoning":{"keep":true},"encrypted_content":"opaque"}
            },
            "tool_output_part_index":0,
            "tool_output_part":{"type":"input_text","text":"opaque tool text"}
        });
        let expected_text = serde_json::to_string(&marker).expect("marker JSON");
        let expected = serde_json::json!({
            "type":"text",
            "text":expected_text,
            "cache_control":{"type":"ephemeral"}
        });
        let mut actual = expected.clone();
        actual["text"] = format!("{}\n", expected["text"].as_str().unwrap()).into();
        assert!(block_matches(&expected, &actual));

        let mut unexpected_outer_field = actual.clone();
        unexpected_outer_field["annotations"] = serde_json::json!({"unbound":true});
        assert!(!block_matches(&expected, &unexpected_outer_field));

        let mut changed_opaque_value = actual;
        let mut parsed: Value =
            serde_json::from_str(changed_opaque_value["text"].as_str().unwrap())
                .expect("marker JSON");
        parsed["item"]["output"]["reasoning"]["keep"] = false.into();
        changed_opaque_value["text"] = serde_json::to_string(&parsed).unwrap().into();
        assert!(!block_matches(&expected, &changed_opaque_value));
    }

    #[test]
    fn normalize_cli_system_messages_appends_text_without_losing_block_metadata() {
        let existing = serde_json::json!({
            "type":"text",
            "text":"EMP system instruction",
            "cache_control":{"type":"ephemeral"}
        });
        let reminder = serde_json::json!({
            "type":"text",
            "text":"synthetic CLI reminder",
            "cache_control":{"type":"ephemeral"}
        });
        let user = serde_json::json!({
            "role":"user",
            "content":[{"type":"text","text":"expected transcript"}]
        });
        let mut body = serde_json::json!({
            "system":[existing],
            "messages":[
                {"role":"system","content":[reminder]},
                user
            ]
        });

        assert!(normalize_cli_system_messages(&mut body));
        assert_eq!(body["system"][0]["text"], "EMP system instruction");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["system"][1]["text"], "synthetic CLI reminder");
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"], serde_json::json!([user]));
    }

    #[test]
    fn normalize_cli_system_messages_rejects_extra_roles_and_non_text_content_atomically() {
        for mut body in [
            serde_json::json!({"messages":[
                {"role":"system","content":"synthetic reminder"},
                {"role":"user","content":"one"},
                {"role":"user","content":"unexpected second user"}
            ]}),
            serde_json::json!({"messages":[
                {"role":"system","content":[{"type":"tool_use","name":"Bash"}]},
                {"role":"user","content":"one"}
            ]}),
            serde_json::json!({"messages":[
                {"role":"assistant","content":"unexpected assistant"},
                {"role":"user","content":"one"}
            ]}),
            serde_json::json!({"messages":[
                {"role":"developer","content":"unexpected role"},
                {"role":"user","content":"one"}
            ]}),
        ] {
            let original = body.clone();
            assert!(!normalize_cli_system_messages(&mut body));
            assert_eq!(
                body, original,
                "rejected normalization must not mutate input"
            );
        }
    }

    #[test]
    fn normalize_cli_system_messages_preserves_the_request_effort_duplicate() {
        let mut body = serde_json::json!({
            "output_config":{"effort":"low"},
            "system":[{"type":"text","text":"EMP instruction"}],
            "messages":[
                {"role":"user","content":"exact transcript"},
                {"role":"system","content":[{
                    "type":"text","text":"Today's date is 2026-09-30.",
                    "cache_control":{"type":"ephemeral"}
                }],"output_config":{"effort":"low"}}
            ]
        });
        assert!(normalize_cli_system_messages(&mut body));
        assert_eq!(body["output_config"], serde_json::json!({"effort":"low"}));
        assert_eq!(
            body["messages"],
            serde_json::json!([
                {"role":"user","content":"exact transcript"}
            ])
        );
        assert_eq!(body["system"][1]["text"], "Today's date is 2026-09-30.");
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");

        for (top, nested) in [
            (serde_json::Value::Null, serde_json::json!({"effort":"low"})),
            (
                serde_json::json!({"effort":"high"}),
                serde_json::json!({"effort":"low"}),
            ),
            (
                serde_json::json!({"effort":"low"}),
                serde_json::json!({"effort":"low","extra":true}),
            ),
            (
                serde_json::json!({"effort":1}),
                serde_json::json!({"effort":1}),
            ),
        ] {
            let mut rejected = serde_json::json!({
                "output_config":top,
                "messages":[
                    {"role":"user","content":"exact transcript"},
                    {"role":"system","content":"date reminder","output_config":nested}
                ]
            });
            let original = rejected.clone();
            assert!(!normalize_cli_system_messages(&mut rejected));
            assert_eq!(rejected, original);
        }
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
