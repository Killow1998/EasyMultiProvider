//! Python-compatible projection for external Responses providers.
//!
//! External gateways receive a complete, stateless history. Plaintext
//! reasoning is never returned to Codex, while explicitly enabled opaque state
//! and bounded summaries can cross the protocol boundary.

use base64::Engine as _;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use url::Url;

pub(crate) const COMPACTION_PREFIX: &str = "emp1:";
pub(crate) const COMPACTION_SUMMARY_PREFIX: &str =
    "Another model produced a continuation summary. Continue from this summary:";
const PORTABLE_TOP_LEVEL: &[&str] = &[
    "model",
    "instructions",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "stop",
    "max_output_tokens",
    "stream",
    "reasoning",
    "text",
    "truncation",
    "service_tier",
];
const PLAINTEXT_REASONING_FIELDS: &[&str] = &[
    "reasoning",
    "reasoning_text",
    "reasoning_content",
    "thinking",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableProjectionError {
    index: usize,
    item_type: String,
    part_types: Vec<String>,
    failure_class: String,
}

impl PortableProjectionError {
    pub(crate) fn new(
        index: usize,
        item_type: &str,
        part_types: Vec<String>,
        failure_class: &str,
    ) -> Self {
        Self {
            index,
            item_type: item_type.chars().take(64).collect(),
            part_types: part_types
                .into_iter()
                .take(16)
                .map(|part| part.chars().take(64).collect())
                .collect(),
            failure_class: failure_class.chars().take(64).collect(),
        }
    }

    pub const fn index(&self) -> usize {
        self.index
    }

    pub fn item_type(&self) -> &str {
        &self.item_type
    }

    pub fn part_types(&self) -> &[String] {
        &self.part_types
    }

    pub fn failure_class(&self) -> &str {
        &self.failure_class
    }
}

impl fmt::Display for PortableProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = if self.part_types.is_empty() {
            "none".to_owned()
        } else {
            self.part_types.join(",")
        };
        write!(
            formatter,
            "request projection failed: index={} type={} parts={} class={}",
            self.index, self.item_type, parts, self.failure_class
        )
    }
}

impl std::error::Error for PortableProjectionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponsesValidationErrorKind {
    NotObject,
    MissingStatus,
    UnknownStatus,
    InvalidOutput,
    InvalidOutputItem,
    UnsupportedOutputItem,
    InvalidMessage,
    InvalidMessageContent,
    UnsupportedMessageContent,
    InvalidToolCall,
    InvalidToolSearch,
    InvalidReasoning,
    InvalidOpaqueOutput,
    InvalidOutputText,
    MissingFailure,
    ContradictoryFailure,
    InvalidIncompleteDetails,
    ContradictoryIncompleteDetails,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesValidationError {
    kind: ResponsesValidationErrorKind,
    message: &'static str,
}

impl ResponsesValidationError {
    const fn new(kind: ResponsesValidationErrorKind, message: &'static str) -> Self {
        Self { kind, message }
    }

    pub const fn kind(self) -> ResponsesValidationErrorKind {
        self.kind
    }

    pub const fn message(self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ResponsesValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ResponsesValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalObservation {
    pub status: u16,
    pub success: bool,
    pub error_class: &'static str,
}

fn error(index: usize, item_type: &str, failure_class: &str) -> PortableProjectionError {
    PortableProjectionError::new(index, item_type, Vec::new(), failure_class)
}

fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn python_string(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None | Some(Value::Null) => fallback.to_owned(),
        Some(Value::String(value)) => value.clone(),
        Some(Value::Bool(value)) => if *value { "True" } else { "False" }.to_owned(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Array(value)) => {
            serde_json::to_string(value).unwrap_or_else(|_| fallback.into())
        }
        Some(Value::Object(value)) => {
            serde_json::to_string(value).unwrap_or_else(|_| fallback.into())
        }
    }
}

fn item_type(item: &Map<String, Value>) -> String {
    match item.get("type") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => "message".to_owned(),
        Some(Value::String(value)) if value.is_empty() => "message".to_owned(),
        value => python_string(value, "message"),
    }
}

fn text_part_types(content: &[Value]) -> Vec<String> {
    content
        .iter()
        .filter_map(|part| object(part).map(|part| python_string(part.get("type"), "unknown")))
        .collect()
}

fn project_content(value: Option<&Value>, index: usize) -> Result<Value, PortableProjectionError> {
    match value {
        Some(Value::String(value)) => Ok(Value::String(value.clone())),
        Some(Value::Array(content)) => {
            let part_types = text_part_types(content);
            let mut projected = Vec::with_capacity(content.len());
            for part in content {
                if let Value::String(value) = part {
                    projected.push(Value::String(value.clone()));
                    continue;
                }
                let Some(part) = object(part) else {
                    return Err(PortableProjectionError::new(
                        index,
                        "message",
                        part_types,
                        "invalid_content_part",
                    ));
                };
                let kind = part.get("type").and_then(Value::as_str);
                if matches!(kind, Some("input_text" | "output_text" | "text"))
                    && part.get("text").is_some_and(Value::is_string)
                {
                    projected.push(json!({"type": kind, "text": part["text"]}));
                    continue;
                }
                if kind == Some("refusal") && part.get("refusal").is_some_and(Value::is_string) {
                    projected.push(json!({"type": "refusal", "refusal": part["refusal"]}));
                    continue;
                }
                if matches!(kind, Some("input_image" | "output_image")) {
                    let image_url = match part.get("image_url") {
                        Some(Value::String(value)) => Some(value.as_str()),
                        Some(Value::Object(value)) => value.get("url").and_then(Value::as_str),
                        _ => None,
                    };
                    if let Some(image_url) = image_url.filter(|value| !value.is_empty()) {
                        let mut result = Map::from_iter([
                            ("type".to_owned(), Value::String(kind.unwrap().to_owned())),
                            ("image_url".to_owned(), Value::String(image_url.to_owned())),
                        ]);
                        if matches!(
                            part.get("detail").and_then(Value::as_str),
                            Some("auto" | "low" | "high" | "original")
                        ) {
                            result.insert("detail".to_owned(), part["detail"].clone());
                        }
                        projected.push(Value::Object(result));
                        continue;
                    }
                }
                return Err(PortableProjectionError::new(
                    index,
                    "message",
                    part_types,
                    "unsupported_content_part",
                ));
            }
            Ok(Value::Array(projected))
        }
        _ => Err(error(index, "message", "invalid_content")),
    }
}

fn instruction_text(
    value: Option<&Value>,
    index: usize,
) -> Result<String, PortableProjectionError> {
    match value {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(Value::Array(content)) => {
            let part_types = content
                .iter()
                .map(|part| match part {
                    Value::String(_) => "text".to_owned(),
                    Value::Object(part) => python_string(part.get("type"), "unknown"),
                    _ => "unknown".to_owned(),
                })
                .collect::<Vec<_>>();
            let mut text = Vec::with_capacity(content.len());
            for part in content {
                match part {
                    Value::String(value) => text.push(value.clone()),
                    Value::Object(part)
                        if matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("input_text" | "output_text" | "text")
                        ) && part.get("text").is_some_and(Value::is_string) =>
                    {
                        text.push(part["text"].as_str().unwrap().to_owned());
                    }
                    _ => {
                        return Err(PortableProjectionError::new(
                            index,
                            "message",
                            part_types,
                            "unsupported_instruction_content",
                        ));
                    }
                }
            }
            Ok(text.join("\n"))
        }
        _ => Err(error(index, "message", "invalid_instruction_content")),
    }
}

fn custom_arguments(value: Option<&Value>) -> Result<String, PortableProjectionError> {
    let input = match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => serde_json::to_string(value)
            .map_err(|_| error(0, "custom_tool_call", "invalid_tool_arguments"))?,
        None => String::new(),
    };
    serde_json::to_string(&json!({"input": input}))
        .map_err(|_| error(0, "custom_tool_call", "invalid_tool_arguments"))
}

pub(crate) fn decode_compaction(value: &str) -> Option<String> {
    // Python translates the URL-safe alphabet before strict decoding, so mixed
    // alphabets are valid too. It does not reject nonzero unused padding bits.
    let encoded = value
        .strip_prefix(COMPACTION_PREFIX)?
        .replace('-', "+")
        .replace('_', "/");
    let engine = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
    );
    let decoded = engine.decode(encoded).ok()?;
    let summary = String::from_utf8(decoded).ok()?;
    (!summary.is_empty()).then_some(summary)
}

fn portable_input(
    source: Option<&Value>,
    preserve_reasoning_state: bool,
) -> Result<(Value, Vec<String>), PortableProjectionError> {
    if matches!(source, None | Some(Value::Null)) {
        return Ok((Value::Null, Vec::new()));
    }
    if let Some(Value::String(value)) = source {
        return Ok((Value::String(value.clone()), Vec::new()));
    }
    let owned;
    let source = match source.unwrap() {
        Value::Object(value) => {
            owned = vec![Value::Object(value.clone())];
            owned.as_slice()
        }
        Value::Array(value) => value.as_slice(),
        _ => return Err(error(0, "input", "invalid_input")),
    };
    let mut projected = Vec::with_capacity(source.len());
    let mut instructions = Vec::new();
    let mut calls = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for (index, raw) in source.iter().enumerate() {
        let Some(item) = object(raw) else {
            return Err(error(index, "unknown", "invalid_item"));
        };
        let kind = item_type(item);
        match kind.as_str() {
            "agent_message" => {
                let content = item.get("content").cloned().unwrap_or_else(|| json!([]));
                let encrypted_part = content.as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part.get("type").and_then(Value::as_str) == Some("encrypted_content")
                    })
                });
                if item.contains_key("encrypted_content") || encrypted_part {
                    return Err(PortableProjectionError::new(
                        index,
                        &kind,
                        vec!["encrypted_content".to_owned()],
                        "encrypted_agent_task_requires_plaintext",
                    ));
                }
                projected.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": project_content(Some(&content), index)?,
                }));
            }
            "reasoning" => {
                if preserve_reasoning_state
                    && item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some_and(|value| !value.is_empty())
                {
                    let mut clean = Map::from_iter([
                        ("type".to_owned(), Value::String("reasoning".to_owned())),
                        (
                            "encrypted_content".to_owned(),
                            item["encrypted_content"].clone(),
                        ),
                    ]);
                    for field in ["id", "status"] {
                        if let Some(value) = item.get(field) {
                            clean.insert(field.to_owned(), value.clone());
                        }
                    }
                    projected.push(Value::Object(clean));
                }
            }
            "additional_tools" => {}
            "item_reference" => {
                return Err(error(index, &kind, "opaque_item_reference"));
            }
            "compaction" => {
                let Some(summary) = item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .and_then(decode_compaction)
                else {
                    let failure = if item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.starts_with(COMPACTION_PREFIX))
                    {
                        "invalid_compaction"
                    } else {
                        "opaque_compaction"
                    };
                    return Err(error(index, &kind, failure));
                };
                projected.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!("{COMPACTION_SUMMARY_PREFIX}\n\n{summary}"),
                    }],
                }));
            }
            "message" => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if matches!(role, "system" | "developer") {
                    let default = Value::String(String::new());
                    instructions.push(instruction_text(
                        item.get("content").or(Some(&default)),
                        index,
                    )?);
                    continue;
                }
                if !matches!(role, "user" | "assistant") {
                    return Err(error(index, &kind, "unsupported_role"));
                }
                let default = Value::String(String::new());
                projected.push(json!({
                    "type": "message",
                    "role": role,
                    "content": project_content(item.get("content").or(Some(&default)), index)?,
                }));
            }
            "function_call" | "custom_tool_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .or_else(|| {
                        item.get("id")
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                    })
                    .ok_or_else(|| error(index, &kind, "invalid_tool_pair"))?;
                let arguments = if kind == "custom_tool_call" {
                    custom_arguments(item.get("input"))?
                } else {
                    match item.get("arguments") {
                        None => "{}".to_owned(),
                        Some(Value::String(value)) => value.clone(),
                        Some(_) => return Err(error(index, &kind, "invalid_tool_arguments")),
                    }
                };
                calls.insert(call_id.to_owned());
                projected.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": python_string(item.get("name"), ""),
                    "arguments": arguments,
                }));
            }
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = item.get("call_id");
                if kind == "function_call_output" && matches!(call_id, None | Some(Value::Null)) {
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| error(index, &kind, "invalid_tool_pair"))?;
                    if !matches!(
                        item.get("namespace"),
                        None | Some(Value::Null | Value::String(_))
                    ) {
                        return Err(error(index, &kind, "invalid_tool_output"));
                    }
                    let output = item
                        .get("output")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new()));
                    if !matches!(output, Value::String(_) | Value::Array(_)) {
                        return Err(error(index, &kind, "invalid_tool_output"));
                    }
                    let mut standalone = Map::from_iter([
                        (
                            "type".to_owned(),
                            Value::String("function_call_output".to_owned()),
                        ),
                        ("name".to_owned(), Value::String(name.to_owned())),
                        ("output".to_owned(), output),
                    ]);
                    if let Some(namespace) = item
                        .get("namespace")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        standalone
                            .insert("namespace".to_owned(), Value::String(namespace.to_owned()));
                    }
                    projected.push(Value::Object(standalone));
                    continue;
                }
                let call_id = call_id
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| error(index, &kind, "invalid_tool_pair"))?;
                if !calls.contains(call_id) {
                    return Err(error(index, &kind, "invalid_tool_pair"));
                }
                if !outputs.insert(call_id.to_owned()) {
                    return Err(error(index, &kind, "duplicate_tool_output"));
                }
                let output = item
                    .get("output")
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new()));
                if !matches!(output, Value::String(_) | Value::Array(_)) {
                    return Err(error(index, &kind, "invalid_tool_output"));
                }
                projected.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                }));
            }
            _ => {
                let part_types = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| text_part_types(parts))
                    .unwrap_or_default();
                return Err(PortableProjectionError::new(
                    index,
                    &kind,
                    part_types,
                    "unsupported_item",
                ));
            }
        }
    }
    Ok((Value::Array(projected), instructions))
}

#[derive(Clone)]
struct RawTool {
    value: Map<String, Value>,
    namespace: Vec<String>,
}

type ToolIdentity = (Vec<String>, String, Map<String, Value>);

fn collect_tools(
    result: &mut Vec<RawTool>,
    source: Option<&Value>,
    namespace: &[String],
) -> Result<(), PortableProjectionError> {
    let Some(source) = source.and_then(Value::as_array) else {
        return Ok(());
    };
    for item in source {
        let Some(item) = object(item) else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) == Some("namespace") {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| error(0, "namespace", "invalid_tool_namespace"))?;
            let mut nested = namespace.to_vec();
            nested.push(name.to_owned());
            collect_tools(result, item.get("tools"), &nested)?;
        } else {
            result.push(RawTool {
                value: item.clone(),
                namespace: namespace.to_vec(),
            });
        }
    }
    Ok(())
}

fn raw_tools(body: &Map<String, Value>) -> Result<Vec<RawTool>, PortableProjectionError> {
    let mut result = Vec::new();
    collect_tools(&mut result, body.get("tools"), &[])?;
    let owned;
    let source: &[Value] = match body.get("input") {
        Some(Value::Object(item)) => {
            owned = vec![Value::Object(item.clone())];
            &owned
        }
        Some(Value::Array(items)) => items,
        _ => &[],
    };
    for item in source {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            collect_tools(&mut result, item.get("tools"), &[])?;
        }
    }
    Ok(result)
}

fn portable_tools(source: Vec<RawTool>) -> Result<Vec<Value>, PortableProjectionError> {
    let mut result = Vec::new();
    let mut seen: BTreeMap<String, ToolIdentity> = BTreeMap::new();
    for (index, item) in source.into_iter().enumerate() {
        let kind = python_string(item.value.get("type"), "unknown");
        if !matches!(kind.as_str(), "function" | "custom") {
            return Err(error(index, &kind, "unsupported_tool_definition"));
        }
        let function = item
            .value
            .get("function")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| item.value.clone());
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| error(index, &kind, "invalid_tool_definition"))?;
        let parameters = if kind == "custom" {
            json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
                "additionalProperties": false,
            })
        } else {
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({}))
        };
        if !parameters.is_object() {
            return Err(error(index, &kind, "invalid_tool_definition"));
        }
        let identity = (item.namespace, kind.clone(), function.clone());
        if let Some(previous) = seen.get(name) {
            if previous != &identity {
                return Err(error(index, &kind, "tool_name_collision"));
            }
            continue;
        }
        seen.insert(name.to_owned(), identity);
        let mut tool = Map::from_iter([
            ("type".to_owned(), Value::String("function".to_owned())),
            ("name".to_owned(), Value::String(name.to_owned())),
            (
                "description".to_owned(),
                Value::String(python_string(function.get("description"), "")),
            ),
            ("parameters".to_owned(), parameters),
        ]);
        if function.get("strict").is_some_and(Value::is_boolean) {
            tool.insert("strict".to_owned(), function["strict"].clone());
        }
        result.push(Value::Object(tool));
    }
    Ok(result)
}

fn official_openai_responses(provider: &Map<String, Value>) -> bool {
    let Some(raw) = provider
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Ok(parsed) = Url::parse(raw) else {
        return false;
    };
    let path = parsed.path().trim_end_matches('/');
    parsed.scheme().eq_ignore_ascii_case("https")
        && parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.port().is_none_or(|port| port == 443)
        && matches!(path, "/v1" | "/v1/responses")
}

pub fn project_request(
    provider: &Map<String, Value>,
    body: &Value,
    preserve_reasoning_state: bool,
) -> Result<Value, PortableProjectionError> {
    let body = object(body).ok_or_else(|| error(0, "input", "invalid_input"))?;
    if body.contains_key("previous_response_id") {
        return Err(error(
            0,
            "previous_response_id",
            "stateful_response_unsupported",
        ));
    }
    let mut projected = Map::new();
    for key in PORTABLE_TOP_LEVEL {
        if let Some(value) = body.get(*key) {
            projected.insert((*key).to_owned(), value.clone());
        }
    }
    if official_openai_responses(provider) {
        for key in ["store", "include", "prompt_cache_key"] {
            if let Some(value) = body.get(key) {
                projected.insert(key.to_owned(), value.clone());
            }
        }
    }
    let (input, hoisted) = portable_input(body.get("input"), preserve_reasoning_state)?;
    projected.insert("input".to_owned(), input);
    let mut instructions = Vec::new();
    match projected.get("instructions") {
        Some(Value::String(value)) => instructions.push(value.clone()),
        None | Some(Value::Null) => {}
        Some(_) => return Err(error(0, "instructions", "invalid_instruction_content")),
    }
    instructions.extend(hoisted);
    if !instructions.is_empty() {
        projected.insert(
            "instructions".to_owned(),
            Value::String(instructions.join("\n\n")),
        );
    }
    let tools = raw_tools(body)?;
    if tools.is_empty() {
        projected.remove("tools");
    } else {
        let tools = portable_tools(tools)?;
        if tools.is_empty() {
            projected.remove("tools");
        } else {
            projected.insert("tools".to_owned(), Value::Array(tools));
        }
    }
    if projected
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("custom")
    {
        projected["tool_choice"]["type"] = Value::String("function".to_owned());
    }
    match projected.get("stream") {
        None | Some(Value::Null) => {
            projected.insert("stream".to_owned(), Value::Bool(false));
        }
        Some(Value::Bool(_)) => {}
        Some(_) => return Err(error(0, "stream", "invalid_stream")),
    }
    Ok(Value::Object(projected))
}

fn custom_tool_ids(raw_id: Option<&Value>, call_id: Option<&Value>) -> (String, String) {
    let paired = call_id
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            raw_id
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("call_unknown");
    let item_id = raw_id
        .and_then(Value::as_str)
        .filter(|value| value.starts_with("ctc_"))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let digest = Sha256::digest(paired.as_bytes());
            format!("ctc_{:x}", digest)[..28].to_owned()
        });
    (item_id, paired.to_owned())
}

fn custom_tool_input(value: Option<&Value>) -> String {
    let value = match value {
        Some(Value::Object(value)) => value
            .get("input")
            .cloned()
            .unwrap_or_else(|| Value::Object(value.clone())),
        Some(Value::String(value)) => {
            let decoded = serde_json::from_str::<Value>(value)
                .unwrap_or_else(|_| Value::String(value.clone()));
            if let Value::Object(object) = &decoded {
                object.get("input").cloned().unwrap_or(decoded)
            } else {
                decoded
            }
        }
        Some(value) => value.clone(),
        None => return String::new(),
    };
    if let Some(value) = value.as_str() {
        value.to_owned()
    } else {
        serde_json::to_string(&value).unwrap_or_default()
    }
}

pub fn custom_tool_names(body: &Value) -> Result<BTreeSet<String>, PortableProjectionError> {
    let body = object(body).ok_or_else(|| error(0, "input", "invalid_input"))?;
    Ok(raw_tools(body)?
        .into_iter()
        .filter(|tool| tool.value.get("type").and_then(Value::as_str) == Some("custom"))
        .filter_map(|tool| {
            tool.value
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .collect())
}

fn project_reasoning_item(
    item: &Map<String, Value>,
    preserve_summary: bool,
    preserve_state: bool,
) -> Option<Value> {
    if !preserve_summary && !preserve_state {
        return None;
    }
    let mut clean = Map::from_iter([("type".to_owned(), Value::String("reasoning".to_owned()))]);
    for field in ["id", "status"] {
        if let Some(value) = item.get(field) {
            clean.insert(field.to_owned(), value.clone());
        }
    }
    if preserve_state
        && item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        clean.insert(
            "encrypted_content".to_owned(),
            item["encrypted_content"].clone(),
        );
    }
    if preserve_summary {
        let summary = item
            .get("summary")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| {
                let part = object(part)?;
                (part.get("type").and_then(Value::as_str) == Some("summary_text")
                    && part.get("text").is_some_and(Value::is_string))
                .then(|| json!({"type": "summary_text", "text": part["text"]}))
            })
            .collect::<Vec<_>>();
        if !summary.is_empty() {
            clean.insert("summary".to_owned(), Value::Array(summary));
        }
    }
    Some(Value::Object(clean))
}

pub fn project_response(
    response: &Value,
    custom_names: &BTreeSet<String>,
    preserve_reasoning_summary: bool,
    preserve_reasoning_state: bool,
) -> Result<Value, PortableProjectionError> {
    let mut projected = object(response)
        .ok_or_else(|| error(0, "output", "invalid_response_output"))?
        .clone();
    for field in PLAINTEXT_REASONING_FIELDS {
        projected.remove(*field);
    }
    let raw_output = projected.get("output").cloned();
    if !matches!(raw_output, None | Some(Value::Null | Value::Array(_))) {
        return Err(error(0, "output", "invalid_response_output"));
    }
    let mut output = Vec::new();
    for (index, raw) in raw_output
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(item) = object(raw) else {
            return Err(error(index, "output", "invalid_response_output"));
        };
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            if let Some(clean) =
                project_reasoning_item(item, preserve_reasoning_summary, preserve_reasoning_state)
            {
                output.push(clean);
            }
            continue;
        }
        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            return Err(error(index, "compaction", "external_compaction"));
        }
        let mut clean = item.clone();
        for field in PLAINTEXT_REASONING_FIELDS {
            clean.remove(*field);
        }
        if let Some(Value::Array(content)) = clean.get_mut("content") {
            content.retain(|part| {
                !part
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| {
                        matches!(
                            kind.to_ascii_lowercase().as_str(),
                            "reasoning" | "reasoning_text" | "thinking" | "thinking_text"
                        )
                    })
            });
        }
        if clean.get("type").and_then(Value::as_str) == Some("function_call")
            && clean
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| custom_names.contains(name))
        {
            let (item_id, call_id) = custom_tool_ids(clean.get("id"), clean.get("call_id"));
            let input = custom_tool_input(clean.remove("arguments").as_ref());
            clean.insert("id".to_owned(), Value::String(item_id));
            clean.insert("call_id".to_owned(), Value::String(call_id));
            clean.insert(
                "type".to_owned(),
                Value::String("custom_tool_call".to_owned()),
            );
            clean.insert("input".to_owned(), Value::String(input));
        }
        output.push(Value::Object(clean));
    }
    if matches!(raw_output, Some(Value::Array(_))) {
        projected.insert("output".to_owned(), Value::Array(output));
    }
    Ok(Value::Object(projected))
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_or_string(value: Option<&Value>) -> String {
    value
        .filter(|value| python_truthy(value))
        .map(|value| python_string(Some(value), ""))
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CustomStreamState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// Request-local projection state for one external Responses SSE stream.
#[derive(Debug, Clone)]
pub struct PortableStreamProjector {
    suppressed_item_ids: BTreeSet<String>,
    custom_names: BTreeSet<String>,
    custom_state: BTreeMap<String, CustomStreamState>,
    preserve_reasoning_summary: bool,
    preserve_reasoning_state: bool,
}

impl PortableStreamProjector {
    pub fn new(
        custom_names: BTreeSet<String>,
        preserve_reasoning_summary: bool,
        preserve_reasoning_state: bool,
    ) -> Self {
        Self {
            suppressed_item_ids: BTreeSet::new(),
            custom_names,
            custom_state: BTreeMap::new(),
            preserve_reasoning_summary,
            preserve_reasoning_state,
        }
    }

    pub fn project(&mut self, event: &Value) -> Result<Option<Value>, PortableProjectionError> {
        let mut projected = object(event)
            .ok_or_else(|| error(0, "event", "invalid_response_output"))?
            .clone();
        let mut event_type = python_or_string(projected.get("type")).to_ascii_lowercase();
        let item = projected.get("item").and_then(Value::as_object).cloned();
        if item
            .as_ref()
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("compaction")
        {
            let index = projected
                .get("output_index")
                .and_then(Value::as_i64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            return Err(error(index, "compaction", "external_compaction"));
        }
        if let Some(item) = item
            .as_ref()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        {
            let item_id = item.get("id").and_then(Value::as_str);
            let clean = project_reasoning_item(
                item,
                self.preserve_reasoning_summary,
                self.preserve_reasoning_state,
            );
            let Some(clean) = clean else {
                if let Some(item_id) = item_id.filter(|value| !value.is_empty()) {
                    self.suppressed_item_ids.insert(item_id.to_owned());
                }
                return Ok(None);
            };
            projected.insert("item".to_owned(), clean);
        }
        if let Some(item) = item.as_ref().filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.custom_names.contains(name))
        }) {
            let raw_item_id = item
                .get("id")
                .filter(|value| python_truthy(value))
                .or_else(|| item.get("call_id").filter(|value| python_truthy(value)))
                .map(|value| python_string(Some(value), ""))
                .unwrap_or_default();
            let raw_id = Value::String(raw_item_id.clone());
            let (item_id, call_id) = custom_tool_ids(Some(&raw_id), item.get("call_id"));
            let state = self
                .custom_state
                .entry(raw_item_id)
                .or_insert_with(|| CustomStreamState {
                    item_id,
                    call_id,
                    name: python_or_string(item.get("name")),
                    arguments: String::new(),
                });
            let mut clean = item.clone();
            clean.insert("id".to_owned(), Value::String(state.item_id.clone()));
            clean.insert("call_id".to_owned(), Value::String(state.call_id.clone()));
            clean.insert(
                "type".to_owned(),
                Value::String("custom_tool_call".to_owned()),
            );
            let arguments = clean
                .remove("arguments")
                .unwrap_or(Value::String(String::new()));
            if python_truthy(&arguments) {
                state.arguments = python_string(Some(&arguments), "");
            }
            clean.insert(
                "input".to_owned(),
                Value::String(custom_tool_input(Some(&Value::String(
                    state.arguments.clone(),
                )))),
            );
            projected.insert("item".to_owned(), Value::Object(clean));
        }
        let raw_item_id = python_or_string(projected.get("item_id"));
        if let Some(state) = self.custom_state.get_mut(&raw_item_id) {
            event_type = python_or_string(projected.get("type"));
            if event_type == "response.function_call_arguments.delta" {
                state
                    .arguments
                    .push_str(&python_or_string(projected.get("delta")));
                return Ok(None);
            }
            if event_type == "response.function_call_arguments.done" {
                if let Some(arguments) = projected
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    state.arguments = arguments.to_owned();
                }
                projected.insert(
                    "type".to_owned(),
                    Value::String("response.custom_tool_call_input.delta".to_owned()),
                );
                projected.insert("item_id".to_owned(), Value::String(state.item_id.clone()));
                projected.remove("arguments");
                projected.insert(
                    "delta".to_owned(),
                    Value::String(custom_tool_input(Some(&Value::String(
                        state.arguments.clone(),
                    )))),
                );
            } else {
                projected.insert("item_id".to_owned(), Value::String(state.item_id.clone()));
            }
        }
        if projected
            .get("item_id")
            .and_then(Value::as_str)
            .is_some_and(|item_id| self.suppressed_item_ids.contains(item_id))
        {
            return Ok(None);
        }
        let summary_event = event_type.starts_with("response.reasoning_summary_");
        if summary_event && !self.preserve_reasoning_summary {
            return Ok(None);
        }
        if !summary_event && (event_type.contains("reasoning") || event_type.contains("thinking")) {
            return Ok(None);
        }
        if projected
            .get("part")
            .and_then(Value::as_object)
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind.to_ascii_lowercase().as_str(),
                    "reasoning" | "reasoning_text" | "thinking" | "thinking_text"
                )
            })
        {
            return Ok(None);
        }
        if let Some(response) = projected.get("response").cloned()
            && response.is_object()
        {
            projected.insert(
                "response".to_owned(),
                project_response(
                    &response,
                    &self.custom_names,
                    self.preserve_reasoning_summary,
                    self.preserve_reasoning_state,
                )?,
            );
        }
        Ok(Some(Value::Object(projected)))
    }
}

fn validation_error(
    kind: ResponsesValidationErrorKind,
    message: &'static str,
) -> ResponsesValidationError {
    ResponsesValidationError::new(kind, message)
}

fn required_response_string(
    item: &Map<String, Value>,
    field: &str,
    kind: ResponsesValidationErrorKind,
    message: &'static str,
) -> Result<(), ResponsesValidationError> {
    if item
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        Ok(())
    } else {
        Err(validation_error(kind, message))
    }
}

fn validate_output_item(item: &Map<String, Value>) -> Result<(), ResponsesValidationError> {
    const INVALID_ITEM: &str = "upstream Responses JSON contains an invalid output item";
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return Err(validation_error(
            ResponsesValidationErrorKind::UnsupportedOutputItem,
            "upstream Responses JSON contains an unsupported output item",
        ));
    };
    if !matches!(
        kind,
        "message"
            | "function_call"
            | "custom_tool_call"
            | "tool_search_call"
            | "reasoning"
            | "compaction"
    ) {
        return Err(validation_error(
            ResponsesValidationErrorKind::UnsupportedOutputItem,
            "upstream Responses JSON contains an unsupported output item",
        ));
    }
    match kind {
        "message" => {
            let Some(content) = item.get("content").and_then(Value::as_array) else {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidMessage,
                    "upstream Responses JSON contains an invalid message item",
                ));
            };
            if item.get("role").and_then(Value::as_str) != Some("assistant") {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidMessage,
                    "upstream Responses JSON contains an invalid message item",
                ));
            }
            for raw in content {
                let Some(part) = object(raw) else {
                    return Err(validation_error(
                        ResponsesValidationErrorKind::InvalidMessageContent,
                        "upstream Responses JSON contains invalid message content",
                    ));
                };
                match part.get("type").and_then(Value::as_str) {
                    Some("output_text")
                        if part.get("text").is_some_and(Value::is_string)
                            && matches!(
                                part.get("annotations"),
                                None | Some(Value::Null | Value::Array(_))
                            ) => {}
                    Some("refusal") if part.get("refusal").is_some_and(Value::is_string) => {}
                    Some("output_text" | "refusal") => {
                        return Err(validation_error(
                            ResponsesValidationErrorKind::InvalidMessageContent,
                            "upstream Responses JSON contains invalid message content",
                        ));
                    }
                    _ => {
                        return Err(validation_error(
                            ResponsesValidationErrorKind::UnsupportedMessageContent,
                            "upstream Responses JSON contains unsupported message content",
                        ));
                    }
                }
            }
        }
        "function_call" => {
            required_response_string(
                item,
                "call_id",
                ResponsesValidationErrorKind::InvalidOutputItem,
                INVALID_ITEM,
            )?;
            required_response_string(
                item,
                "name",
                ResponsesValidationErrorKind::InvalidOutputItem,
                INVALID_ITEM,
            )?;
            let valid_arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .is_some_and(|value| value.is_object());
            if !valid_arguments {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidToolCall,
                    "Responses upstream returned invalid tool arguments",
                ));
            }
        }
        "custom_tool_call" => {
            for field in ["call_id", "name", "input"] {
                required_response_string(
                    item,
                    field,
                    ResponsesValidationErrorKind::InvalidOutputItem,
                    INVALID_ITEM,
                )?;
            }
        }
        "tool_search_call" => {
            required_response_string(
                item,
                "call_id",
                ResponsesValidationErrorKind::InvalidOutputItem,
                INVALID_ITEM,
            )?;
            if item.get("execution").and_then(Value::as_str) != Some("client")
                || !item.get("arguments").is_some_and(Value::is_object)
            {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidToolSearch,
                    "upstream Responses JSON contains invalid tool search",
                ));
            }
        }
        "reasoning" => {
            let valid_summary = match item.get("summary") {
                None | Some(Value::Null) => true,
                Some(Value::Array(parts)) => parts.iter().all(|raw| {
                    object(raw).is_some_and(|part| {
                        part.get("type").and_then(Value::as_str) == Some("summary_text")
                            && part.get("text").is_some_and(Value::is_string)
                    })
                }),
                Some(_) => false,
            };
            let valid_opaque = matches!(
                item.get("encrypted_content"),
                None | Some(Value::Null | Value::String(_))
            );
            if !valid_summary || !valid_opaque {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidReasoning,
                    "upstream Responses JSON contains invalid reasoning output",
                ));
            }
        }
        "compaction" => {
            required_response_string(
                item,
                "encrypted_content",
                ResponsesValidationErrorKind::InvalidOpaqueOutput,
                INVALID_ITEM,
            )?;
        }
        _ => unreachable!("output kind was checked above"),
    }
    Ok(())
}

pub fn validate_responses_body(
    value: &Value,
    validate_output_items: bool,
) -> Result<(), ResponsesValidationError> {
    let Some(response) = object(value) else {
        return Err(validation_error(
            ResponsesValidationErrorKind::NotObject,
            "upstream Responses JSON is not an object",
        ));
    };
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            validation_error(
                ResponsesValidationErrorKind::MissingStatus,
                "upstream Responses JSON has no response status",
            )
        })?;
    if !matches!(status, "completed" | "incomplete" | "failed") {
        return Err(validation_error(
            ResponsesValidationErrorKind::UnknownStatus,
            "upstream Responses JSON has an unknown response status",
        ));
    }
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            validation_error(
                ResponsesValidationErrorKind::InvalidOutput,
                "upstream Responses JSON has no valid output",
            )
        })?;
    for raw in output {
        let Some(item) = object(raw) else {
            return Err(validation_error(
                ResponsesValidationErrorKind::InvalidOutputItem,
                "upstream Responses JSON contains an invalid output item",
            ));
        };
        if validate_output_items {
            validate_output_item(item)?;
        }
    }
    if response.contains_key("output_text")
        && !response.get("output_text").is_some_and(Value::is_string)
    {
        return Err(validation_error(
            ResponsesValidationErrorKind::InvalidOutputText,
            "upstream Responses JSON has invalid output text",
        ));
    }
    let upstream_error = response.get("error");
    if status == "failed" {
        if upstream_error
            .and_then(Value::as_object)
            .is_none_or(Map::is_empty)
        {
            return Err(validation_error(
                ResponsesValidationErrorKind::MissingFailure,
                "upstream Responses JSON has a failed status without an error",
            ));
        }
    } else if !matches!(upstream_error, None | Some(Value::Null))
        && !upstream_error.is_some_and(|value| value.as_object().is_some_and(Map::is_empty))
    {
        return Err(validation_error(
            ResponsesValidationErrorKind::ContradictoryFailure,
            "upstream Responses JSON has a contradictory error",
        ));
    }
    let incomplete = response.get("incomplete_details");
    if status == "incomplete" {
        if !matches!(incomplete, None | Some(Value::Null | Value::Object(_))) {
            return Err(validation_error(
                ResponsesValidationErrorKind::InvalidIncompleteDetails,
                "upstream Responses JSON has invalid incomplete details",
            ));
        }
    } else if !matches!(incomplete, None | Some(Value::Null))
        && !incomplete.is_some_and(|value| value.as_object().is_some_and(Map::is_empty))
    {
        return Err(validation_error(
            ResponsesValidationErrorKind::ContradictoryIncompleteDetails,
            "upstream Responses JSON has contradictory incomplete details",
        ));
    }
    Ok(())
}

pub fn terminal_observation(
    value: &Value,
    validate_output_items: bool,
) -> Result<TerminalObservation, ResponsesValidationError> {
    validate_responses_body(value, validate_output_items)?;
    let response = value.as_object().expect("validated Responses object");
    Ok(
        match response["status"].as_str().expect("validated status") {
            "completed" => TerminalObservation {
                status: 200,
                success: true,
                error_class: "none",
            },
            "incomplete" => {
                let reason = response
                    .get("incomplete_details")
                    .and_then(Value::as_object)
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                TerminalObservation {
                    status: 200,
                    success: false,
                    error_class: match reason {
                        "max_output_tokens" => "output_limit",
                        "content_filter" => "content_filter",
                        _ => "stream_incomplete",
                    },
                }
            }
            "failed" => TerminalObservation {
                status: 502,
                success: false,
                error_class: "stream_error",
            },
            _ => unreachable!("validated terminal status"),
        },
    )
}
