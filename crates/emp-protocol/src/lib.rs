//! Bounded Chat Completions response projection for the Rust router.
//!
//! The API intentionally keeps `serde_json::Value` as its wire type.  It
//! preserves unknown upstream fields separately from the canonical Responses
//! projection and reports protocol failures without embedding payload text.

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
pub use chat_request::responses_to_chat;
pub mod anthropic_projection;
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

fn combined_array_text(value: &Value, invalid: &'static str) -> Result<String, ProtocolError> {
    let parts = value.as_array().ok_or_else(|| protocol_error(invalid))?;
    let mut result = String::new();
    for part in parts {
        let Some(part) = object(part) else {
            return Err(protocol_error(invalid));
        };
        if !matches!(
            part.get("type").and_then(Value::as_str),
            Some("text" | "output_text")
        ) {
            return Err(protocol_error(invalid));
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            return Err(protocol_error(invalid));
        };
        result.push_str(text);
    }
    Ok(result)
}

fn message_text(message: &Map<String, Value>) -> Result<String, ProtocolError> {
    match message.get("content") {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(value @ Value::Array(_)) => combined_array_text(
            value,
            "Chat Completions upstream returned invalid message content",
        ),
        Some(_) => Err(protocol_error(
            "Chat Completions upstream returned invalid message content",
        )),
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

fn tool_arguments(value: Option<&Value>) -> Result<String, ProtocolError> {
    match value {
        Some(Value::String(value)) => match serde_json::from_str::<Value>(value) {
            Ok(Value::Object(_)) => Ok(value.clone()),
            _ => Err(protocol_error(
                "Chat Completions upstream returned invalid tool arguments",
            )),
        },
        None | Some(_) => Err(protocol_error(
            "Chat Completions upstream returned invalid tool arguments",
        )),
    }
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

fn custom_tool_input(arguments: &str) -> String {
    let parsed = serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_owned()));
    let value = if let Value::Object(object) = &parsed {
        object.get("input").unwrap_or(&parsed)
    } else {
        &parsed
    };
    if let Value::String(value) = value {
        return value.clone();
    }
    serde_json::to_string(value).unwrap_or_else(|_| arguments.to_owned())
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

fn tool_item(
    call: &Map<String, Value>,
    custom_names: &BTreeSet<String>,
) -> Result<Value, ProtocolError> {
    let raw_id = string_at(call, "id").unwrap_or_default();
    let name = call
        .get("function")
        .and_then(object)
        .and_then(|function| string_at(function, "name"))
        .unwrap_or_default();
    if raw_id.is_empty() || name.is_empty() {
        return Err(protocol_error(
            "Chat Completions upstream returned an invalid tool call",
        ));
    }
    let arguments = tool_arguments(
        call.get("function")
            .and_then(object)
            .and_then(|function| function.get("arguments")),
    )?;
    let mut item = if custom_names.contains(name) {
        serde_json::json!({
            "id": custom_tool_id(raw_id),
            "type": "custom_tool_call",
            "status": "completed",
            "call_id": raw_id,
            "name": name,
            "input": custom_tool_input(&arguments),
        })
    } else {
        serde_json::json!({
            "id": raw_id,
            "type": "function_call",
            "status": "completed",
            "call_id": raw_id,
            "name": name,
            "arguments": arguments,
        })
    };
    if let Some(extra) = call.get("extra_content").filter(|extra| extra.is_object()) {
        item["extra_content"] = extra.clone();
    }
    Ok(item)
}

/// Project one complete Chat Completions response into Responses JSON.
pub fn response_from_chat(
    value: &Value,
    requested_model: &str,
    custom_names: &[&str],
    ids: &ChatIds,
) -> Result<ChatProjection, ProtocolError> {
    let Some(root) = object(value) else {
        return Err(protocol_error(
            "Chat Completions upstream returned an invalid response",
        ));
    };
    if root.get("error").is_some_and(json_truthy) {
        return Err(upstream_error(
            "Chat Completions upstream returned an error",
        ));
    }
    let choice = root
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(object)
        .ok_or_else(|| protocol_error("Chat Completions upstream returned an invalid response"))?;
    let message = choice
        .get("message")
        .and_then(object)
        .ok_or_else(|| protocol_error("Chat Completions upstream returned an invalid response"))?;
    let custom_names = custom_names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    let mut output = Vec::new();
    let reasoning = chat_reasoning_text(message);
    if !reasoning.is_empty() {
        output.push(reasoning_item(&ids.reasoning, reasoning));
    }
    let text = message_text(message)?;
    let refusal = refusal_text(message)?;
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(serde_json::json!({
            "type": "output_text", "text": text, "annotations": []
        }));
    }
    if !refusal.is_empty() {
        content.push(serde_json::json!({"type": "refusal", "refusal": refusal}));
    }
    if !content.is_empty() {
        output.push(serde_json::json!({
            "id": ids.message,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": content,
        }));
    }
    let calls = match message.get("tool_calls") {
        None => &[][..],
        Some(Value::Array(calls)) => calls.as_slice(),
        Some(_) => {
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid tool call",
            ));
        }
    };
    let mut seen_ids = BTreeSet::new();
    for call in calls {
        let Some(call) = object(call) else {
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid tool call",
            ));
        };
        let raw_id = string_at(call, "id").unwrap_or_default();
        if raw_id.is_empty() || !seen_ids.insert(raw_id) {
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid tool call",
            ));
        }
        output.push(tool_item(call, &custom_names)?);
    }
    let reason = incomplete_reason(choice.get("finish_reason"))?;
    let mut response = serde_json::json!({
        "id": ids.response,
        "object": "response",
        "status": if reason.is_some() { "incomplete" } else { "completed" },
        "model": requested_model,
        "output": output,
        "output_text": text,
    });
    if let Some(reason) = reason {
        response["incomplete_details"] = serde_json::json!({"reason": reason});
    }
    if let Some(usage) = root.get("usage").filter(|usage| !usage.is_null()) {
        response["usage"] = projected_usage(usage)?;
    }
    if let Some(service_tier) = string_at(root, "service_tier") {
        response["service_tier"] = Value::String(service_tier.to_owned());
    }
    let mut unknown_fields = Vec::new();
    collect_unknown_fields(&mut unknown_fields, value);
    Ok(ChatProjection {
        response,
        unknown_fields,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFrame {
    Sse,
    Ordinary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub event: &'static str,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    OutputText,
    Refusal,
}

impl ContentKind {
    fn event_name(self) -> &'static str {
        match self {
            Self::OutputText => "output_text",
            Self::Refusal => "refusal",
        }
    }

    fn field_name(self) -> &'static str {
        match self {
            Self::OutputText => "text",
            Self::Refusal => "refusal",
        }
    }
}

#[derive(Debug, Clone)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
    extra_content: Option<Value>,
}

/// Bounded state machine for Chat Completions chunks, SSE deltas and
/// gateways that ignore `stream: true` and return one ordinary response.
#[derive(Clone)]
pub struct ChatStream {
    response_id: String,
    message_id: String,
    reasoning_id: String,
    late_reasoning_id: String,
    sequence: u64,
    response: Map<String, Value>,
    reasoning: String,
    late_reasoning: String,
    content_parts: Vec<(ContentKind, String)>,
    tool_calls: BTreeMap<i64, ToolCallState>,
    tool_call_ids: BTreeSet<String>,
    custom_names: BTreeSet<String>,
    usage: Option<Value>,
    finish_reason: Option<Value>,
    saw_output: bool,
    saw_done: bool,
    started: bool,
    finished: bool,
    ordinary_complete: bool,
    message_started: bool,
    message_closed: bool,
    reasoning_started: bool,
    stream_text_bytes: usize,
    unknown_fields: Vec<UnknownField>,
}

impl fmt::Debug for ChatStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatStream")
            .field("response_id", &self.response_id)
            .field("message_started", &self.message_started)
            .field("message_closed", &self.message_closed)
            .field("reasoning_started", &self.reasoning_started)
            .field("saw_done", &self.saw_done)
            .field("unknown_fields", &self.unknown_fields.len())
            .finish()
    }
}

impl ChatStream {
    pub fn new(response_model: impl Into<String>, ids: ChatIds) -> Self {
        let response_model = response_model.into();
        let response = serde_json::json!({
            "id": ids.response.clone(),
            "object": "response",
            "status": "in_progress",
            "model": response_model,
            "output": []
        });
        Self {
            response_id: ids.response,
            message_id: ids.message,
            reasoning_id: ids.reasoning,
            late_reasoning_id: ids.late_reasoning,
            sequence: 0,
            response: response.as_object().expect("literal response").clone(),
            reasoning: String::new(),
            late_reasoning: String::new(),
            content_parts: Vec::new(),
            tool_calls: BTreeMap::new(),
            tool_call_ids: BTreeSet::new(),
            custom_names: BTreeSet::new(),
            usage: None,
            finish_reason: None,
            saw_output: false,
            saw_done: false,
            started: false,
            finished: false,
            ordinary_complete: false,
            message_started: false,
            message_closed: false,
            reasoning_started: false,
            stream_text_bytes: 0,
            unknown_fields: Vec::new(),
        }
    }

    pub fn with_custom_names(mut self, custom_names: &[&str]) -> Self {
        self.custom_names = custom_names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    pub fn start_event(&mut self) -> Result<StreamEvent, ProtocolError> {
        if self.started || self.finished {
            return Err(protocol_error("Chat Completions stream already started"));
        }
        self.started = true;
        let response = Value::Object(self.response.clone());
        Ok(self.event(
            "response.created",
            serde_json::json!({"type": "response.created", "response": response}),
        ))
    }

    pub fn unknown_fields(&self) -> &[UnknownField] {
        &self.unknown_fields
    }

    pub fn mark_done(&mut self) {
        self.saw_done = true;
    }

    pub fn ordinary_complete(&self) -> bool {
        self.ordinary_complete
    }

    pub fn push(
        &mut self,
        chunk: &Value,
        frame: ChatFrame,
    ) -> Result<Vec<StreamEvent>, ProtocolError> {
        if !self.started || self.finished || self.saw_done {
            return Err(protocol_error(
                "Chat Completions stream is not accepting chunks",
            ));
        }
        let mut events = Vec::new();
        collect_unknown_fields(&mut self.unknown_fields, chunk);
        let Some(root) = object(chunk) else {
            return Err(protocol_error(
                "Chat Completions upstream returned malformed SSE data",
            ));
        };
        if root.get("error").is_some_and(json_truthy) {
            return Err(upstream_error(
                "Chat Completions upstream returned an error",
            ));
        }
        if let Some(usage) = root.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(projected_usage(usage)?);
        }
        if let Some(service_tier) = string_at(root, "service_tier") {
            self.response.insert(
                "service_tier".to_owned(),
                Value::String(service_tier.to_owned()),
            );
        }
        let choice = root
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .cloned();
        let Some(choice) = choice else {
            if self.usage.is_some() {
                return Ok(events);
            }
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid stream chunk",
            ));
        };
        let choice = object(&choice).ok_or_else(|| {
            protocol_error("Chat Completions upstream returned an invalid stream chunk")
        })?;
        if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
            self.finish_reason = Some(reason.clone());
        }
        let mut delta = choice
            .get("delta")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        if object(&delta).is_none_or(Map::is_empty)
            && let Some(message) = choice.get("message").filter(|value| value.is_object())
        {
            delta = message.clone();
            self.ordinary_complete = frame == ChatFrame::Ordinary
                && choice
                    .get("finish_reason")
                    .is_some_and(|value| !value.is_null());
        }
        let delta = object(&delta).ok_or_else(|| {
            protocol_error("Chat Completions upstream returned an invalid stream chunk")
        })?;
        self.consume_reasoning(delta, &mut events)?;
        self.consume_content(delta, &mut events)?;
        self.consume_tool_calls(delta)?;
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProtocolError> {
        if !self.started || self.finished {
            return Err(protocol_error(
                "Chat Completions stream cannot finish twice",
            ));
        }
        let mut events = Vec::new();
        if !self.saw_done && !self.ordinary_complete {
            return Err(stream_error(
                "Chat Completions upstream ended before [DONE]",
            ));
        }
        if !self.saw_output {
            return Err(stream_error(
                "upstream returned an empty Chat Completions response",
            ));
        }
        let reason = incomplete_reason(self.finish_reason.as_ref())?;
        self.finished = true;
        let message_index = usize::from(self.reasoning_started);
        if self.reasoning_started && !self.message_started {
            events.push(self.item_done(0, &reasoning_item(&self.reasoning_id, &self.reasoning)));
        }
        if self.message_started {
            let parts = self.content_parts.clone();
            for (content_index, (kind, value)) in parts.iter().enumerate() {
                let event = match kind {
                    ContentKind::OutputText => "response.output_text.done",
                    ContentKind::Refusal => "response.refusal.done",
                };
                events.push(self.message_event(
                    event,
                    message_index,
                    content_index,
                    kind.field_name(),
                    value,
                ));
                events.push(self.content_part_event(
                    "response.content_part.done",
                    message_index,
                    content_index,
                    *kind,
                    value,
                ));
            }
            events.push(self.item_done(message_index, &self.message_output()));
        }
        let late_index = message_index + usize::from(self.message_started);
        if !self.late_reasoning.is_empty() {
            let text = self.late_reasoning.clone();
            events.push(self.item_added(
                late_index,
                &serde_json::json!({
                    "id": self.late_reasoning_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                    "content": []
                }),
            ));
            let late_reasoning_id = self.late_reasoning_id.clone();
            events.push(self.reasoning_event(
                "response.reasoning_text.delta",
                &late_reasoning_id,
                late_index,
                &text,
            ));
            events
                .push(self.item_done(late_index, &reasoning_item(&self.late_reasoning_id, &text)));
        }
        let tool_base = late_index + usize::from(!self.late_reasoning.is_empty());
        let mut function_outputs = Vec::new();
        for (position, (_, state)) in std::mem::take(&mut self.tool_calls).into_iter().enumerate() {
            if state.name.is_empty() {
                return Err(upstream_error(
                    "upstream returned a tool call without a name",
                ));
            }
            let arguments = tool_arguments(Some(&Value::String(state.arguments.clone())))?;
            let custom = self.custom_names.contains(&state.name);
            let item_id = if custom {
                custom_tool_id(&state.id)
            } else {
                state.id.clone()
            };
            let output_index = tool_base + position;
            let call_type = if custom {
                "custom_tool_call"
            } else {
                "function_call"
            };
            let mut added = serde_json::json!({
                "id": item_id,
                "type": call_type,
                "status": "in_progress",
                "call_id": state.id,
                "name": state.name,
            });
            if custom {
                added["input"] = Value::String(String::new());
            } else {
                added["arguments"] = Value::String(String::new());
            }
            if let Some(extra) = &state.extra_content {
                added["extra_content"] = extra.clone();
            }
            events.push(self.item_added(output_index, &added));
            let projected_input = if custom {
                custom_tool_input(&arguments)
            } else {
                arguments.clone()
            };
            let mut output = serde_json::json!({
                "id": item_id,
                "type": call_type,
                "status": "completed",
                "call_id": state.id,
                "name": state.name,
            });
            if custom {
                output["input"] = Value::String(projected_input.clone());
            } else {
                output["arguments"] = Value::String(arguments.clone());
            }
            if let Some(extra) = state.extra_content {
                output["extra_content"] = extra;
            }
            if custom {
                events.push(self.tool_delta_event(
                    "response.custom_tool_call_input.delta",
                    output_index,
                    &item_id,
                    &projected_input,
                ));
            } else {
                events.push(self.tool_delta_event(
                    "response.function_call_arguments.delta",
                    output_index,
                    &item_id,
                    &arguments,
                ));
                events.push(self.event(
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "arguments": arguments,
                    }),
                ));
            }
            function_outputs.push(output.clone());
            events.push(self.item_done(output_index, &output));
        }
        let mut outputs = Vec::new();
        if self.reasoning_started {
            outputs.push(reasoning_item(&self.reasoning_id, &self.reasoning));
        }
        if self.message_started {
            outputs.push(self.message_output());
        }
        if !self.late_reasoning.is_empty() {
            outputs.push(reasoning_item(
                &self.late_reasoning_id,
                &self.late_reasoning,
            ));
        }
        outputs.extend(function_outputs);
        self.response
            .insert("output".to_owned(), Value::Array(outputs));
        let output_text = self
            .content_parts
            .iter()
            .find(|(kind, _)| *kind == ContentKind::OutputText)
            .map_or_else(String::new, |(_, value)| value.clone());
        self.response
            .insert("output_text".to_owned(), Value::String(output_text));
        self.response.insert(
            "status".to_owned(),
            Value::String(
                if reason.is_some() {
                    "incomplete"
                } else {
                    "completed"
                }
                .to_owned(),
            ),
        );
        if let Some(reason) = reason {
            self.response.insert(
                "incomplete_details".to_owned(),
                serde_json::json!({"reason": reason}),
            );
        }
        if let Some(usage) = self.usage.clone() {
            self.response.insert("usage".to_owned(), usage);
        }
        let event = if reason.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let response = Value::Object(self.response.clone());
        events.push(self.event(
            event,
            serde_json::json!({"type": event, "response": response}),
        ));
        Ok(events)
    }

    fn event(&mut self, event: &'static str, mut value: Value) -> StreamEvent {
        self.sequence += 1;
        if let Some(object) = value.as_object_mut() {
            object.insert("sequence_number".to_owned(), Value::from(self.sequence));
        }
        StreamEvent { event, value }
    }

    fn item_added(&mut self, output_index: usize, item: &Value) -> StreamEvent {
        self.event(
            "response.output_item.added",
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item
            }),
        )
    }

    fn item_done(&mut self, output_index: usize, item: &Value) -> StreamEvent {
        self.event(
            "response.output_item.done",
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item
            }),
        )
    }

    fn reasoning_event(
        &mut self,
        event: &'static str,
        item_id: &str,
        output_index: usize,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": value
            }),
        )
    }

    fn message_event(
        &mut self,
        event: &'static str,
        output_index: usize,
        content_index: usize,
        field: &str,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": content_index,
                field: value
            }),
        )
    }

    fn tool_delta_event(
        &mut self,
        event: &'static str,
        output_index: usize,
        item_id: &str,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": item_id,
                "output_index": output_index,
                "delta": value
            }),
        )
    }

    fn content_part(kind: ContentKind, value: &str) -> Value {
        let mut part = Map::new();
        part.insert(
            "type".to_owned(),
            Value::String(kind.event_name().to_owned()),
        );
        part.insert(
            kind.field_name().to_owned(),
            Value::String(value.to_owned()),
        );
        if kind == ContentKind::OutputText {
            part.insert("annotations".to_owned(), Value::Array(Vec::new()));
        }
        Value::Object(part)
    }

    fn content_part_event(
        &mut self,
        event: &'static str,
        output_index: usize,
        content_index: usize,
        kind: ContentKind,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": content_index,
                "part": Self::content_part(kind, value)
            }),
        )
    }

    fn message_output(&self) -> Value {
        let content = self
            .content_parts
            .iter()
            .map(|(kind, value)| Self::content_part(*kind, value))
            .collect::<Vec<_>>();
        serde_json::json!({
            "id": self.message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": content
        })
    }

    fn consume_reasoning(
        &mut self,
        delta: &Map<String, Value>,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProtocolError> {
        let piece = chat_reasoning_text(delta);
        if piece.is_empty() {
            return Ok(());
        }
        self.add_stream_text(piece)?;
        self.saw_output = true;
        if self.message_started {
            self.late_reasoning.push_str(piece);
            return Ok(());
        }
        if !self.reasoning_started {
            self.reasoning_started = true;
            events.push(self.item_added(
                0,
                &serde_json::json!({
                    "id": self.reasoning_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                    "content": []
                }),
            ));
        }
        self.reasoning.push_str(piece);
        let reasoning_id = self.reasoning_id.clone();
        events.push(self.reasoning_event("response.reasoning_text.delta", &reasoning_id, 0, piece));
        Ok(())
    }

    fn consume_content(
        &mut self,
        delta: &Map<String, Value>,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProtocolError> {
        let text = match delta.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(value @ Value::Array(_)) => combined_array_text(
                value,
                "Chat Completions upstream returned invalid message content",
            )?,
            Some(_) => {
                return Err(protocol_error(
                    "Chat Completions upstream returned invalid message content",
                ));
            }
        };
        let refusal = refusal_text(delta)?;
        for (kind, fragment) in [
            (ContentKind::OutputText, text),
            (ContentKind::Refusal, refusal),
        ] {
            if fragment.is_empty() {
                continue;
            }
            if self.message_closed {
                return Err(protocol_error(
                    "Chat Completions upstream returned content after a tool call",
                ));
            }
            self.add_stream_text(&fragment)?;
            self.saw_output = true;
            if !self.message_started {
                if self.reasoning_started {
                    events.push(
                        self.item_done(0, &reasoning_item(&self.reasoning_id, &self.reasoning)),
                    );
                }
                self.message_started = true;
                let message_index = usize::from(self.reasoning_started);
                events.push(self.item_added(
                    message_index,
                    &serde_json::json!({
                        "id": self.message_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    }),
                ));
            }
            let message_index = usize::from(self.reasoning_started);
            let content_index = self.content_index(kind);
            if content_index == self.content_parts.len() {
                self.content_parts.push((kind, fragment.clone()));
                events.push(self.content_part_event(
                    "response.content_part.added",
                    message_index,
                    content_index,
                    kind,
                    "",
                ));
            } else {
                self.content_parts[content_index].1.push_str(&fragment);
            }
            let event = match kind {
                ContentKind::OutputText => "response.output_text.delta",
                ContentKind::Refusal => "response.refusal.delta",
            };
            events.push(self.message_event(
                event,
                message_index,
                content_index,
                "delta",
                &fragment,
            ));
        }
        Ok(())
    }

    fn content_index(&self, kind: ContentKind) -> usize {
        self.content_parts
            .iter()
            .position(|(existing, _)| *existing == kind)
            .unwrap_or(self.content_parts.len())
    }

    fn consume_tool_calls(&mut self, delta: &Map<String, Value>) -> Result<(), ProtocolError> {
        let calls = match delta.get("tool_calls") {
            None => return Ok(()),
            Some(Value::Array(calls)) => calls,
            Some(_) => {
                return Err(protocol_error(
                    "Chat Completions upstream returned an invalid tool call",
                ));
            }
        };
        if calls.is_empty() {
            return Ok(());
        }
        self.saw_output = true;
        if self.message_started {
            self.message_closed = true;
        }
        for raw_call in calls {
            let Some(raw_call) = object(raw_call) else {
                return Err(protocol_error(
                    "Chat Completions upstream returned an invalid tool call",
                ));
            };
            let index = tool_index(raw_call.get("index"), self.tool_calls.len())?;
            let empty_function = Map::new();
            let function = match raw_call.get("function") {
                None | Some(Value::Null) => &empty_function,
                Some(Value::Object(function)) => function,
                Some(_) => {
                    return Err(protocol_error(
                        "Chat Completions upstream returned an invalid tool call",
                    ));
                }
            };
            let is_new = !self.tool_calls.contains_key(&index);
            let raw_id = string_at(raw_call, "id").unwrap_or_default();
            if is_new {
                if raw_id.is_empty() {
                    return Err(protocol_error(
                        "Chat Completions upstream returned an invalid tool call",
                    ));
                }
                if !self.tool_call_ids.insert(raw_id.to_owned()) {
                    return Err(protocol_error(
                        "Chat Completions upstream returned a duplicate tool call ID",
                    ));
                }
            }
            let state = self
                .tool_calls
                .entry(index)
                .or_insert_with(|| ToolCallState {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    extra_content: None,
                });
            if state.id.is_empty() {
                state.id = raw_id.to_owned();
            } else if !raw_id.is_empty() && raw_id != state.id {
                return Err(protocol_error(
                    "Chat Completions upstream returned a duplicate tool call ID",
                ));
            }
            if let Some(name) = function.get("name") {
                let Some(name) = name.as_str() else {
                    return Err(protocol_error(
                        "Chat Completions upstream returned an invalid tool call",
                    ));
                };
                state.name.push_str(name);
            }
            if let Some(extra) = raw_call
                .get("extra_content")
                .filter(|extra| extra.is_object())
            {
                state.extra_content = Some(extra.clone());
            }
            if let Some(arguments) = function.get("arguments") {
                let Some(arguments) = arguments.as_str() else {
                    return Err(protocol_error(
                        "Chat Completions upstream returned invalid tool arguments",
                    ));
                };
                state.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    fn add_stream_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        let total = self
            .stream_text_bytes
            .checked_add(text.len())
            .ok_or_else(|| upstream_error("upstream streamed text is too large"))?;
        if total > MAX_STREAM_TEXT_BYTES {
            return Err(upstream_error("upstream streamed text is too large"));
        }
        self.stream_text_bytes = total;
        Ok(())
    }
}
