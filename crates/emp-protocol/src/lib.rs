//! Request and response projection between Codex and external model protocols.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const MAX_STREAM_TEXT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    InvalidRequest,
    UpstreamRejected,
    ProtocolError,
    RequestTooLarge,
    StreamIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    kind: ProtocolErrorKind,
    message: &'static str,
}

impl ProtocolError {
    fn new(kind: ProtocolErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub fn kind(&self) -> ProtocolErrorKind {
        self.kind
    }

    pub fn message(&self) -> &'static str {
        self.message
    }

    pub fn status(&self) -> u16 {
        match self.kind {
            ProtocolErrorKind::InvalidRequest => 422,
            _ => 502,
        }
    }

    pub fn error_class(&self) -> &'static str {
        match self.kind {
            ProtocolErrorKind::InvalidRequest => "invalid_request",
            ProtocolErrorKind::UpstreamRejected => "upstream_error",
            ProtocolErrorKind::ProtocolError => "protocol_error",
            ProtocolErrorKind::RequestTooLarge | ProtocolErrorKind::StreamIncomplete => {
                "stream_incomplete"
            }
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ProtocolError {}

fn protocol_error(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorKind::ProtocolError, message)
}

fn request_error(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorKind::InvalidRequest, message)
}

mod chat_request;
mod chat_text;
mod tool_definitions;
pub use chat_request::responses_to_chat;
pub mod anthropic_projection;
pub mod collaboration;
pub mod context_error;
pub mod native_responses;
pub mod portable_responses;

fn upstream_error(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorKind::UpstreamRejected, message)
}

fn stream_error(message: &'static str) -> ProtocolError {
    ProtocolError::new(ProtocolErrorKind::StreamIncomplete, message)
}

#[derive(Clone, PartialEq)]
pub struct UnknownField {
    pub path: String,
    pub value: Value,
}

impl fmt::Debug for UnknownField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UnknownField")
            .field("path", &self.path)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatProjection {
    pub response: Value,
    pub unknown_fields: Vec<UnknownField>,
}

/// Request-local identifiers supplied by the router's secure ID source.
///
/// Requiring these values keeps protocol projection deterministic in tests and
/// prevents a shared fixed identifier from crossing concurrent Codex turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatIds {
    response: String,
    message: String,
    reasoning: String,
    late_reasoning: String,
}

impl ChatIds {
    pub fn new(
        response: impl Into<String>,
        message: impl Into<String>,
        reasoning: impl Into<String>,
        late_reasoning: impl Into<String>,
    ) -> Result<Self, ProtocolError> {
        let result = Self {
            response: response.into(),
            message: message.into(),
            reasoning: reasoning.into(),
            late_reasoning: late_reasoning.into(),
        };
        for (value, prefix) in [
            (&result.response, "resp_"),
            (&result.message, "msg_"),
            (&result.reasoning, "rs_"),
            (&result.late_reasoning, "rs_"),
        ] {
            if value.len() > 128
                || !value.starts_with(prefix)
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            {
                return Err(protocol_error("invalid generated response identifier"));
            }
        }
        if result.reasoning == result.late_reasoning {
            return Err(protocol_error(
                "generated response identifiers are not unique",
            ));
        }
        Ok(result)
    }
}

const TOP_LEVEL_KEYS: &[&str] = &[
    "id",
    "object",
    "created",
    "model",
    "choices",
    "usage",
    "system_fingerprint",
    "service_tier",
];
const CHOICE_KEYS: &[&str] = &["index", "message", "finish_reason", "logprobs"];
const MESSAGE_KEYS: &[&str] = &[
    "role",
    "content",
    "refusal",
    "tool_calls",
    "reasoning_content",
    "reasoning",
    "reasoning_text",
];
const TOOL_CALL_KEYS: &[&str] = &["index", "id", "type", "function", "extra_content"];
const FUNCTION_KEYS: &[&str] = &["name", "arguments"];
const USAGE_KEYS: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "total_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
    "prompt_cache_hit_tokens",
];
const USAGE_DETAIL_KEYS: &[&str] = &[
    "cached_tokens",
    "reasoning_tokens",
    "cache_creation_tokens",
    "cache_creation_1h_tokens",
];

fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn string_at<'a>(value: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

fn push_unknown(
    result: &mut Vec<UnknownField>,
    path: &str,
    value: &Map<String, Value>,
    known: &[&str],
) {
    for (key, item) in value {
        if known.contains(&key.as_str()) {
            continue;
        }
        result.push(UnknownField {
            path: format!("{path}.{key}"),
            value: item.clone(),
        });
    }
}

fn collect_usage_unknown(result: &mut Vec<UnknownField>, usage: &Value) {
    let Some(usage) = object(usage) else {
        return;
    };
    push_unknown(result, "$.usage", usage, USAGE_KEYS);
    for (path, key) in [
        ("$.usage.prompt_tokens_details", "prompt_tokens_details"),
        (
            "$.usage.completion_tokens_details",
            "completion_tokens_details",
        ),
    ] {
        if let Some(details) = usage.get(key).and_then(object) {
            push_unknown(result, path, details, USAGE_DETAIL_KEYS);
        }
    }
}

fn collect_unknown_fields(result: &mut Vec<UnknownField>, value: &Value) {
    let Some(root) = object(value) else {
        return;
    };
    push_unknown(result, "$", root, TOP_LEVEL_KEYS);
    for (index, choice) in root
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(choice) = object(choice) else {
            continue;
        };
        push_unknown(result, &format!("$.choices[{index}]"), choice, CHOICE_KEYS);
        let Some(message) = choice.get("message").and_then(object) else {
            continue;
        };
        let path = format!("$.choices[{index}].message");
        push_unknown(result, &path, message, MESSAGE_KEYS);
        for (tool_index, call) in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let Some(call) = object(call) else {
                continue;
            };
            let call_path = format!("{path}.tool_calls[{tool_index}]");
            push_unknown(result, &call_path, call, TOOL_CALL_KEYS);
            if let Some(function) = call.get("function").and_then(object) {
                push_unknown(
                    result,
                    &format!("{call_path}.function"),
                    function,
                    FUNCTION_KEYS,
                );
            }
        }
    }
    if let Some(usage) = root.get("usage") {
        collect_usage_unknown(result, usage);
    }
}

fn chat_reasoning_text(message: &Map<String, Value>) -> &str {
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        if let Some(value) = message.get(field).and_then(Value::as_str)
            && !value.is_empty()
        {
            return value;
        }
    }
    ""
}

fn message_text(message: &Map<String, Value>) -> Result<String, ProtocolError> {
    let invalid = || protocol_error("Chat Completions upstream returned invalid message content");
    match message.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(Value::Array(parts)) => {
            let mut result = String::new();
            for part in parts {
                let part = object(part).ok_or_else(invalid)?;
                if !matches!(string_at(part, "type"), Some("text" | "output_text")) {
                    return Err(invalid());
                }
                result.push_str(string_at(part, "text").ok_or_else(invalid)?);
            }
            Ok(result)
        }
        Some(_) => Err(invalid()),
    }
}

fn refusal_text(value: &Map<String, Value>) -> Result<String, ProtocolError> {
    match value.get("refusal") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(protocol_error(
            "Chat Completions upstream returned invalid refusal",
        )),
    }
}

fn tool_arguments(value: &str) -> Result<Map<String, Value>, ProtocolError> {
    serde_json::from_str(value)
        .map_err(|_| protocol_error("Chat Completions upstream returned invalid tool arguments"))
}

fn tool_index(value: Option<&Value>, default: usize) -> Result<i64, ProtocolError> {
    let invalid = || protocol_error("Chat Completions upstream returned an invalid tool call");
    match value {
        None => i64::try_from(default).map_err(|_| invalid()),
        Some(Value::Number(number)) => {
            if let Some(value) = number.as_i64() {
                return Ok(value);
            }
            let value = number.as_f64().ok_or_else(invalid)?;
            if !value.is_finite() || value < i64::MIN as f64 || value > i64::MAX as f64 {
                return Err(invalid());
            }
            Ok(value.trunc() as i64)
        }
        Some(Value::String(value)) => value.trim().parse::<i64>().map_err(|_| invalid()),
        Some(_) => Err(invalid()),
    }
}

fn custom_tool_input(mut arguments: Map<String, Value>) -> String {
    match arguments
        .remove("input")
        .unwrap_or(Value::Object(arguments))
    {
        Value::String(value) => value,
        value => value.to_string(),
    }
}

fn custom_tool_id(call_id: &str) -> String {
    if call_id.starts_with("ctc_") {
        return call_id.to_owned();
    }
    let digest = Sha256::digest(call_id.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("ctc_{suffix}")
}

fn incomplete_reason(value: Option<&Value>) -> Result<Option<&'static str>, ProtocolError> {
    let Some(value) = value else {
        return Err(protocol_error(
            "Chat Completions upstream returned no finish reason",
        ));
    };
    let Some(value) = value.as_str() else {
        return Err(protocol_error(
            "Chat Completions upstream returned an unknown finish reason",
        ));
    };
    match value.to_ascii_lowercase().as_str() {
        "stop" | "tool_calls" | "function_call" => Ok(None),
        "length" | "max_tokens" | "max_output_tokens" => Ok(Some("max_output_tokens")),
        "content_filter" | "content_filtered" | "safety" => Ok(Some("content_filter")),
        _ => Err(protocol_error(
            "Chat Completions upstream returned an unknown finish reason",
        )),
    }
}

fn projected_usage(usage: &Value) -> Result<Value, ProtocolError> {
    let Some(usage) = object(usage) else {
        return Err(protocol_error(
            "Chat Completions upstream returned invalid usage",
        ));
    };
    let mut result = Map::new();
    for (source, target) in [
        ("prompt_tokens", "input_tokens"),
        ("completion_tokens", "output_tokens"),
        ("total_tokens", "total_tokens"),
    ] {
        if let Some(value) = usage.get(source) {
            result.insert(target.to_owned(), value.clone());
        }
    }
    for (source, target, detail) in [
        (
            "prompt_tokens_details",
            "input_tokens_details",
            "cached_tokens",
        ),
        (
            "completion_tokens_details",
            "output_tokens_details",
            "reasoning_tokens",
        ),
    ] {
        if let Some(value) = usage
            .get(source)
            .and_then(object)
            .and_then(|details| details.get(detail))
        {
            let mut details = Map::new();
            details.insert(detail.to_owned(), value.clone());
            result.insert(target.to_owned(), Value::Object(details));
        }
    }
    if let Some(value) = usage.get("prompt_cache_hit_tokens") {
        let details = result
            .entry("input_tokens_details".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(details) = details.as_object_mut() {
            details
                .entry("cached_tokens".to_owned())
                .or_insert_with(|| value.clone());
        }
    }
    if let Some(details) = usage.get("prompt_tokens_details").and_then(object) {
        let target = result
            .entry("input_tokens_details".to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(target) = target.as_object_mut() {
            for key in ["cache_creation_tokens", "cache_creation_1h_tokens"] {
                if let Some(value) = details.get(key) {
                    target.insert(key.to_owned(), value.clone());
                }
            }
        }
    }
    Ok(Value::Object(result))
}

fn reasoning_item(id: &str, text: &str) -> Value {
    serde_json::json!({
        "id": id,
        "type": "reasoning",
        "status": "completed",
        "summary": [],
        "content": [{"type": "reasoning_text", "text": text}]
    })
}

mod chat_response;
pub use chat_response::response_from_chat;
mod chat_stream;
pub use chat_stream::{ChatFrame, ChatStream, StreamEvent};

pub mod tool_bridge;
