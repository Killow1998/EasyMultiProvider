//! Bounded Responses <-> Anthropic Messages projections.

use base64::Engine as _;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

const COMPACTION_PREFIX: &str = "emp1:";
const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started this task and produced a continuation summary. Use it to continue without repeating completed work:";
const ANTHROPIC_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];
const TEXT_PART_TYPES: [&str; 3] = ["input_text", "output_text", "text"];
const OMITTED_INPUT_TYPES: [&str; 2] = ["reasoning", "additional_tools"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicErrorKind {
    Request,
    HistoryReconstruction,
    Upstream,
    StreamIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicError {
    pub kind: AnthropicErrorKind,
    pub message: &'static str,
}

impl std::fmt::Display for AnthropicError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for AnthropicError {}

impl AnthropicError {
    pub const fn status(&self) -> u16 {
        match self.kind {
            AnthropicErrorKind::Request => 422,
            AnthropicErrorKind::HistoryReconstruction => 409,
            AnthropicErrorKind::Upstream | AnthropicErrorKind::StreamIncomplete => 502,
        }
    }

    pub const fn error_class(&self) -> &'static str {
        match self.kind {
            AnthropicErrorKind::Request => "invalid_request",
            AnthropicErrorKind::HistoryReconstruction => "history_reconstruction_failed",
            AnthropicErrorKind::Upstream => "protocol_error",
            AnthropicErrorKind::StreamIncomplete => "stream_incomplete",
        }
    }
}

fn request_error(message: &'static str) -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::Request,
        message,
    }
}

fn upstream_error(message: &'static str) -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::Upstream,
        message,
    }
}

fn history_error() -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::HistoryReconstruction,
        message: "history_reconstruction_failed: reason=history_projection_incomplete; Codex history was not modified",
    }
}

fn stream_error(message: &'static str) -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::StreamIncomplete,
        message,
    }
}

pub fn anthropic_error_kind(error: &AnthropicError) -> AnthropicErrorKind {
    error.kind
}

fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

fn required_string<'a>(
    value: Option<&'a Value>,
    message: &'static str,
) -> Result<&'a str, AnthropicError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(request_error(message))
}

fn required_upstream_string<'a>(
    value: Option<&'a Value>,
    message: &'static str,
) -> Result<&'a str, AnthropicError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(upstream_error(message))
}

fn request_text(value: Option<&Value>, field: &str) -> Result<String, AnthropicError> {
    let invalid = move || {
        request_error(match field {
            "instructions" => "request projection failed: invalid instructions",
            "tool output" => "request projection failed: invalid tool output",
            "standalone tool output" => "request projection failed: invalid standalone tool output",
            _ => "request projection failed: invalid message content",
        })
    };
    let unsupported = move || {
        request_error(match field {
            "instructions" => "request projection failed: unsupported instructions",
            "tool output" => "request projection failed: unsupported tool output",
            "standalone tool output" => {
                "request projection failed: unsupported standalone tool output"
            }
            _ => "request projection failed: unsupported message content",
        })
    };
    match value {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut result = String::new();
            for part in parts {
                match part {
                    Value::String(text) => result.push_str(text),
                    Value::Object(part) if matches!(part.get("type").and_then(Value::as_str), Some(kind) if TEXT_PART_TYPES.contains(&kind)) =>
                    {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(invalid)?;
                        result.push_str(text);
                    }
                    _ => return Err(unsupported()),
                }
            }
            Ok(result)
        }
        _ => Err(invalid()),
    }
}

fn standalone_tool_output(item: &Map<String, Value>) -> Result<Option<String>, AnthropicError> {
    if item.get("type").and_then(Value::as_str) != Some("function_call_output")
        || item.get("call_id").is_some_and(|value| !value.is_null())
    {
        return Ok(None);
    }
    let name = required_string(
        item.get("name"),
        "request projection failed: invalid standalone tool output name",
    )?;
    let namespace = match item.get("namespace") {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value,
        Some(_) => {
            return Err(request_error(
                "request projection failed: invalid standalone tool output namespace",
            ));
        }
    };
    let empty = Value::String(String::new());
    let output = request_text(
        item.get("output").or(Some(&empty)),
        "standalone tool output",
    )?;
    let label = if namespace.is_empty() {
        name.to_owned()
    } else {
        format!("{namespace}/{name}")
    };
    Ok(Some(format!(
        "Standalone tool output from {label}:\n{output}"
    )))
}

fn decode_compaction(item: &Map<String, Value>) -> Option<String> {
    let encoded = item
        .get("encrypted_content")
        .and_then(Value::as_str)?
        .strip_prefix(COMPACTION_PREFIX)?;
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(encoded)
        .ok()?;
    String::from_utf8(decoded)
        .ok()
        .filter(|summary| !summary.is_empty())
}

fn agent_message_text(item: &Map<String, Value>) -> Result<String, AnthropicError> {
    required_string(
        item.get("author"),
        "request projection failed: invalid agent message author",
    )?;
    required_string(
        item.get("recipient"),
        "request projection failed: invalid agent message recipient",
    )?;
    let parts = item
        .get("content")
        .and_then(Value::as_array)
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| request_error("request projection failed: invalid agent message content"))?;
    let mut text = Vec::with_capacity(parts.len());
    for part in parts {
        let Some(part) = object(part) else {
            return Err(request_error(
                "request projection failed: invalid agent message content",
            ));
        };
        if part.get("type").and_then(Value::as_str) != Some("input_text") {
            return Err(request_error(
                "request projection failed: unsupported agent message content",
            ));
        }
        text.push(
            required_string(
                part.get("text"),
                "request projection failed: invalid agent message content",
            )?
            .to_owned(),
        );
    }
    Ok(text.join("\n"))
}

fn request_tool_arguments(value: Option<&Value>) -> Result<Value, AnthropicError> {
    if let Some(Value::Object(value)) = value {
        return Ok(Value::Object(value.clone()));
    }
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| request_error("request projection failed: invalid tool arguments"))?;
    match serde_json::from_str::<Value>(value) {
        Ok(Value::Object(value)) => Ok(Value::Object(value)),
        _ => Err(request_error(
            "request projection failed: invalid tool arguments",
        )),
    }
}

fn custom_tool_arguments(value: Option<&Value>) -> Result<Value, AnthropicError> {
    let input = match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => serde_json::to_string(value)
            .map_err(|_| request_error("request projection failed: invalid custom tool input"))?,
        None => String::new(),
    };
    serde_json::to_value(serde_json::json!({"input": input}))
        .map_err(|_| request_error("request projection failed: invalid custom tool input"))
}

fn parse_data_image(value: &str) -> Result<Option<Value>, AnthropicError> {
    let Some(rest) = value.strip_prefix("data:") else {
        return Ok(None);
    };
    let Some((header, data)) = rest.split_once(',') else {
        return Err(request_error("request projection failed: invalid image"));
    };
    let mut header_parts = header.split(';');
    let Some(media_type) = header_parts.next() else {
        return Err(request_error("request projection failed: invalid image"));
    };
    let Some(encoding) = header_parts.next() else {
        return Err(request_error("request projection failed: invalid image"));
    };
    if header_parts.next().is_some() || !encoding.eq_ignore_ascii_case("base64") {
        return Err(request_error("request projection failed: invalid image"));
    }
    if !ANTHROPIC_IMAGE_MEDIA_TYPES.contains(&media_type.to_ascii_lowercase().as_str()) {
        return Err(request_error(
            "request projection failed: unsupported Anthropic image type",
        ));
    }
    let invalid = || request_error("request projection failed: invalid image");
    if data.is_empty() || data.len() % 4 != 0 {
        return Err(invalid());
    }
    if !data
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/' || byte == b'=')
        || data.ends_with("===")
    {
        return Err(invalid());
    }
    let _ = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| invalid())?;
    Ok(Some(serde_json::json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type.to_ascii_lowercase(), "data": data}
    })))
}

fn anthropic_content(value: Option<&Value>) -> Result<Vec<Value>, AnthropicError> {
    if let Some(Value::String(text)) = value {
        return Ok(vec![serde_json::json!({"type": "text", "text": text})]);
    }
    let Some(Value::Array(parts)) = value else {
        return Err(request_error(
            "request projection failed: invalid Anthropic content",
        ));
    };
    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        if let Value::String(text) = part {
            result.push(serde_json::json!({"type": "text", "text": text}));
            continue;
        }
        let Some(part) = object(part) else {
            return Err(request_error(
                "request projection failed: invalid Anthropic content",
            ));
        };
        let part_type = part.get("type").and_then(Value::as_str);
        if part_type == Some("refusal") {
            let refusal = part
                .get("refusal")
                .and_then(Value::as_str)
                .ok_or_else(|| request_error("request projection failed: invalid refusal"))?;
            result.push(serde_json::json!({"type": "text", "text": refusal}));
            continue;
        }
        if let Some(kind) = part_type.filter(|kind| TEXT_PART_TYPES.contains(kind)) {
            let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                request_error("request projection failed: invalid Anthropic content")
            })?;
            result.push(serde_json::json!({"type": "text", "text": text}));
            let _ = kind;
            continue;
        }
        if part_type != Some("input_image") {
            return Err(request_error(
                "request projection failed: unsupported Anthropic content",
            ));
        }
        let image_url = match part.get("image_url") {
            Some(Value::String(value)) => Some(value.as_str()),
            Some(Value::Object(value)) => value.get("url").and_then(Value::as_str),
            _ => None,
        }
        .filter(|url| !url.is_empty())
        .ok_or_else(|| request_error("request projection failed: invalid image"))?;
        if let Some(image) = parse_data_image(image_url)? {
            result.push(image);
            continue;
        }
        let Some((scheme, remainder)) = image_url.split_once("://") else {
            return Err(request_error(
                "request projection failed: unsupported image URL",
            ));
        };
        let host_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") || host_end == 0 {
            return Err(request_error(
                "request projection failed: unsupported image URL",
            ));
        }
        result.push(serde_json::json!({
            "type": "image",
            "source": {"type": "url", "url": image_url}
        }));
    }
    Ok(result)
}

struct RawTool<'a> {
    item: &'a Map<String, Value>,
    namespace: Vec<String>,
}

fn raw_tools(body: &Map<String, Value>) -> Result<Vec<RawTool<'_>>, AnthropicError> {
    fn visit<'a>(
        value: Option<&'a Value>,
        namespace: &[String],
        result: &mut Vec<RawTool<'a>>,
    ) -> Result<(), AnthropicError> {
        let Some(Value::Array(items)) = value else {
            return Ok(());
        };
        for item in items {
            if let Some(item) = object(item) {
                if item.get("type").and_then(Value::as_str) == Some("namespace") {
                    let name = required_string(
                        item.get("name"),
                        "request projection failed: invalid tool namespace",
                    )?;
                    let mut nested = namespace.to_vec();
                    nested.push(name.to_owned());
                    visit(item.get("tools"), &nested, result)?;
                } else {
                    result.push(RawTool {
                        item,
                        namespace: namespace.to_vec(),
                    });
                }
            }
        }
        Ok(())
    }
    let mut result = Vec::new();
    visit(body.get("tools"), &[], &mut result)?;
    let input = match body.get("input") {
        Some(Value::Object(item)) => vec![item],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    };
    for item in input {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            visit(item.get("tools"), &[], &mut result)?;
        }
    }
    Ok(result)
}

fn python_description(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(value) => value.to_string(),
    }
}

fn tools(body: &Map<String, Value>) -> Result<Vec<Value>, AnthropicError> {
    let mut result = Vec::new();
    let mut seen: BTreeMap<String, Value> = BTreeMap::new();
    for raw in raw_tools(body)? {
        let item = raw.item;
        let tool_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(tool_type, "function" | "custom") {
            return Err(request_error(
                "request projection failed: unsupported tool definition",
            ));
        }
        let function = item.get("function").and_then(object).unwrap_or(item);
        let name = required_string(
            function.get("name"),
            "request projection failed: invalid tool definition",
        )?;
        let parameters = if tool_type == "custom" {
            serde_json::json!({
                "type": "object", "properties": {"input": {"type": "string"}},
                "required": ["input"], "additionalProperties": false
            })
        } else {
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()))
        };
        if !parameters.is_object() {
            return Err(request_error(
                "request projection failed: invalid tool definition",
            ));
        }
        let identity = json!([raw.namespace, tool_type, Value::Object(function.clone())]);
        if let Some(previous) = seen.get(name) {
            if previous != &identity {
                return Err(request_error(
                    "request projection failed: tool name collision",
                ));
            }
            continue;
        }
        seen.insert(name.to_owned(), identity);
        result.push(serde_json::json!({
            "name": name,
            "description": python_description(function.get("description")),
            "input_schema": parameters,
        }));
    }
    Ok(result)
}

fn tool_choice(value: &Value, parallel: Option<&Value>) -> Result<Value, AnthropicError> {
    let choice = match value {
        Value::String(value) if value == "auto" => serde_json::json!({"type": "auto"}),
        Value::String(value) if value == "required" => serde_json::json!({"type": "any"}),
        Value::String(value) if value == "none" => serde_json::json!({"type": "none"}),
        Value::Object(value)
            if matches!(
                value.get("type").and_then(Value::as_str),
                Some("function" | "custom")
            ) =>
        {
            let name = required_string(
                value.get("name"),
                "request projection failed: unsupported tool choice",
            )?;
            serde_json::json!({"type": "tool", "name": name})
        }
        _ => {
            return Err(request_error(
                "request projection failed: unsupported tool choice",
            ));
        }
    };
    if parallel == Some(&Value::Bool(false)) {
        let mut choice = choice;
        choice["disable_parallel_tool_use"] = Value::Bool(true);
        return Ok(choice);
    }
    Ok(choice)
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn anthropic_max_tokens(value: Option<&Value>) -> Result<Value, AnthropicError> {
    if !python_truthy(value) {
        return Ok(Value::from(4096));
    }
    match value {
        Some(Value::Bool(true)) => Ok(Value::from(1)),
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => {
            Ok(Value::Number(number.clone()))
        }
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|number| number.is_finite())
            .map(|number| Value::from(number.trunc() as i64))
            .ok_or_else(|| request_error("request projection failed: invalid max output tokens")),
        Some(Value::String(value)) => value
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| request_error("request projection failed: invalid max output tokens")),
        _ => Err(request_error(
            "request projection failed: invalid max output tokens",
        )),
    }
}

fn json_schema_format(body: &Map<String, Value>) -> Result<Option<Value>, AnthropicError> {
    let Some(text) = body.get("text") else {
        return Ok(None);
    };
    let text = object(text)
        .ok_or_else(|| request_error("request projection failed: invalid text controls"))?;
    let Some(format) = text.get("format") else {
        return Ok(None);
    };
    let format = object(format)
        .filter(|format| format.get("type").and_then(Value::as_str) == Some("json_schema"))
        .ok_or_else(|| {
            request_error("request projection failed: unsupported structured output format")
        })?;
    let name = format
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let schema = format.get("schema").filter(|schema| schema.is_object());
    let strict = match format.get("strict") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Err(request_error(
                "request projection failed: invalid structured output format",
            ));
        }
    };
    let (Some(_), Some(schema)) = (name, schema) else {
        return Err(request_error(
            "request projection failed: invalid structured output format",
        ));
    };
    let _ = strict;
    Ok(Some(
        serde_json::json!({"type": "json_schema", "schema": schema.clone()}),
    ))
}

fn messages(body: &Map<String, Value>) -> Result<Vec<Value>, AnthropicError> {
    let empty_message = Value::String(String::new());
    let source = body.get("input").unwrap_or(&empty_message);
    let source = match source {
        Value::String(text) => {
            vec![serde_json::json!({"type": "message", "role": "user", "content": text})]
        }
        Value::Object(item) => vec![Value::Object(item.clone())],
        Value::Array(items) => items.clone(),
        _ => return Err(request_error("request projection failed: invalid input")),
    };
    let mut normalized = Vec::with_capacity(source.len());
    for item in source {
        let Some(item) = object(&item) else {
            return Err(request_error(
                "request projection failed: invalid input item",
            ));
        };
        if item.get("type").and_then(Value::as_str) == Some("compaction") {
            let summary = decode_compaction(item).ok_or_else(history_error)?;
            normalized.push(serde_json::json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": format!("{COMPACTION_SUMMARY_PREFIX}\n\n{summary}")}]
            }));
        } else {
            normalized.push(item.clone().into());
        }
    }

    let mut result = Vec::new();
    let mut pending_calls: Vec<Value> = Vec::new();
    let mut pending_results: Vec<Value> = Vec::new();
    let mut call_ids = BTreeSet::new();
    let mut output_ids = BTreeSet::new();
    fn flush(messages: &mut Vec<Value>, calls: &mut Vec<Value>, results: &mut Vec<Value>) {
        if !calls.is_empty() {
            messages
                .push(serde_json::json!({"role": "assistant", "content": std::mem::take(calls)}));
        }
        if !results.is_empty() {
            messages.push(serde_json::json!({"role": "user", "content": std::mem::take(results)}));
        }
    }

    for raw in normalized {
        let Some(item) = object(&raw) else {
            return Err(request_error(
                "request projection failed: invalid input item",
            ));
        };
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        match item_type {
            "message" => {
                flush(&mut result, &mut pending_calls, &mut pending_results);
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if !matches!(role, "user" | "assistant") {
                    return Err(request_error(
                        "request projection failed: unsupported Anthropic role",
                    ));
                }
                let content = anthropic_content(item.get("content").or(Some(&empty_message)))?;
                if content.is_empty() {
                    return Err(request_error(
                        "request projection failed: empty Anthropic content",
                    ));
                }
                result.push(serde_json::json!({"role": role, "content": content}));
            }
            "agent_message" => {
                flush(&mut result, &mut pending_calls, &mut pending_results);
                result.push(serde_json::json!({
                    "role": "user", "content": [{"type": "text", "text": agent_message_text(item)?}]
                }));
            }
            "function_call" | "custom_tool_call" => {
                if !pending_results.is_empty() {
                    result.push(serde_json::json!({"role": "user", "content": std::mem::take(&mut pending_results)}));
                }
                let arguments = if item_type == "custom_tool_call" {
                    custom_tool_arguments(item.get("input"))?
                } else {
                    let default_arguments = Value::String("{}".to_owned());
                    request_tool_arguments(item.get("arguments").or(Some(&default_arguments)))?
                };
                let call_id = required_string(
                    item.get("call_id")
                        .filter(|value| value.as_str().is_some_and(|value| !value.is_empty()))
                        .or_else(|| item.get("id")),
                    "request projection failed: invalid tool call ID",
                )?;
                if !call_ids.insert(call_id.to_owned()) {
                    return Err(request_error(
                        "request projection failed: duplicate tool call ID",
                    ));
                }
                let name = required_string(
                    item.get("name"),
                    "request projection failed: invalid tool name",
                )?;
                pending_calls.push(serde_json::json!({
                    "type": "tool_use", "id": call_id, "name": name, "input": arguments
                }));
            }
            "function_call_output" | "custom_tool_call_output" => {
                if !pending_calls.is_empty() {
                    result.push(serde_json::json!({"role": "assistant", "content": std::mem::take(&mut pending_calls)}));
                }
                if let Some(standalone) = standalone_tool_output(item)? {
                    if !pending_results.is_empty() {
                        result.push(serde_json::json!({"role": "user", "content": std::mem::take(&mut pending_results)}));
                    }
                    result.push(serde_json::json!({"role": "user", "content": [{"type": "text", "text": standalone}]}));
                    continue;
                }
                let call_id = required_string(
                    item.get("call_id"),
                    "request projection failed: invalid tool call ID",
                )?;
                if !call_ids.contains(call_id) || !output_ids.insert(call_id.to_owned()) {
                    return Err(request_error(
                        "request projection failed: invalid tool output pairing",
                    ));
                }
                let empty_output = Value::String(String::new());
                pending_results.push(serde_json::json!({
                    "type": "tool_result", "tool_use_id": call_id,
                    "content": request_text(item.get("output").or(Some(&empty_output)), "tool output")?
                }));
            }
            kind if OMITTED_INPUT_TYPES.contains(&kind) => {}
            _ => {
                flush(&mut result, &mut pending_calls, &mut pending_results);
                return Err(request_error(
                    "request projection failed: unsupported input item",
                ));
            }
        }
    }
    flush(&mut result, &mut pending_calls, &mut pending_results);
    Ok(result)
}

mod response;
pub use response::response_from_anthropic;

pub fn responses_to_anthropic(body: &Value, upstream_model: &str) -> Result<Value, AnthropicError> {
    let Some(body) = object(body) else {
        return Err(request_error(
            "request projection failed: body must be an object",
        ));
    };
    let mut payload = serde_json::json!({
        "model": upstream_model,
        "max_tokens": anthropic_max_tokens(body.get("max_output_tokens"))?,
        "messages": messages(body)?,
        "stream": python_truthy(body.get("stream")),
    });
    if let Some(instructions) = body.get("instructions") {
        payload["system"] = Value::String(request_text(Some(instructions), "instructions")?);
    }
    let projected_tools = tools(body)?;
    if !projected_tools.is_empty() {
        payload["tools"] = Value::Array(projected_tools);
    }
    for source in ["temperature", "top_p"] {
        if let Some(value) = body.get(source) {
            payload[source] = value.clone();
        }
    }
    if let Some(stop) = body.get("stop") {
        payload["stop_sequences"] = if stop.is_array() {
            stop.clone()
        } else {
            serde_json::json!([stop])
        };
    }
    let parallel = body.get("parallel_tool_calls");
    if parallel.is_some_and(|value| !value.is_boolean()) {
        return Err(request_error(
            "request projection failed: parallel tool calls must be boolean",
        ));
    }
    if let Some(choice) = body.get("tool_choice") {
        payload["tool_choice"] = tool_choice(choice, parallel)?;
    } else if parallel == Some(&Value::Bool(false)) {
        payload["tool_choice"] = tool_choice(&Value::String("auto".to_owned()), parallel)?;
    }
    let mut output_config = Map::new();
    if let Some(format) = json_schema_format(body)? {
        output_config.insert("format".to_owned(), format);
    }
    if let Some(effort) = body
        .get("reasoning")
        .and_then(object)
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
    {
        if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
            return Err(request_error(
                "request projection failed: unsupported Anthropic reasoning effort",
            ));
        }
        output_config.insert("effort".to_owned(), Value::String(effort.to_owned()));
    }
    if !output_config.is_empty() {
        payload["output_config"] = Value::Object(output_config);
    }
    Ok(payload)
}

mod stream;
pub use stream::{AnthropicIds, AnthropicStream};
