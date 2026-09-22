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

fn token_count(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) if number.is_u64() => {
            number.as_u64().filter(|value| *value <= 10_000_000)
        }
        _ => None,
    }
}

fn anthropic_usage(usage: &Value) -> Value {
    let Some(usage) = object(usage) else {
        return Value::Object(Map::new());
    };
    let mut result = Map::new();
    let output = token_count(usage.get("output_tokens"));
    if let Some(output) = output {
        result.insert("output_tokens".to_owned(), json!(output));
    }
    let total = token_count(usage.get("input_tokens"));
    let has_cache = usage.contains_key("cache_read_input_tokens")
        || usage.contains_key("cache_creation_input_tokens");
    if has_cache {
        let read = token_count(usage.get("cache_read_input_tokens"));
        let written = token_count(usage.get("cache_creation_input_tokens"));
        if let (Some(total), Some(read), Some(written)) = (total, read, written) {
            let mut details = Map::new();
            details.insert("cached_tokens".to_owned(), json!(read));
            details.insert("cache_creation_tokens".to_owned(), json!(written));
            if let Some(creation) = usage.get("cache_creation").and_then(object)
                && creation.contains_key("ephemeral_1h_input_tokens")
            {
                details.insert(
                    "cache_creation_1h_tokens".to_owned(),
                    creation["ephemeral_1h_input_tokens"].clone(),
                );
            }
            result.insert("input_tokens_details".to_owned(), Value::Object(details));
            let total = total + read + written;
            result.insert("input_tokens".to_owned(), json!(total));
            if let Some(output) = output {
                result.insert("total_tokens".to_owned(), json!(total + output));
            }
        }
    } else if let Some(total) = total {
        result.insert("input_tokens".to_owned(), json!(total));
        if let Some(output) = output {
            result.insert("total_tokens".to_owned(), json!(total + output));
        }
    }
    Value::Object(result)
}

fn incomplete_reason(value: Option<&Value>) -> Result<Option<&'static str>, AnthropicError> {
    let Some(reason) = value.and_then(Value::as_str) else {
        return Err(upstream_error("Anthropic upstream returned no stop reason"));
    };
    match reason.to_ascii_lowercase().as_str() {
        "end_turn" | "tool_use" | "stop_sequence" => Ok(None),
        "max_tokens" | "max_output_tokens" => Ok(Some("max_output_tokens")),
        "content_filter" | "content_filtered" | "safety" | "refusal" => Ok(Some("content_filter")),
        _ => Err(upstream_error(
            "Anthropic upstream returned an unknown stop reason",
        )),
    }
}

fn tool_arguments(value: Option<&Value>) -> Result<String, AnthropicError> {
    let default = Value::Object(Map::new());
    let Some(value) = object(value.unwrap_or(&default)) else {
        return Err(upstream_error(
            "Anthropic upstream returned invalid tool input",
        ));
    };
    serde_json::to_string(&Value::Object(value.clone()))
        .map_err(|_| upstream_error("Anthropic upstream returned invalid tool input"))
}

fn custom_tool_id(call_id: &str) -> String {
    if call_id.starts_with("ctc_") {
        return call_id.to_owned();
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(call_id.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("ctc_{suffix}")
}

fn custom_tool_input(arguments: &str) -> String {
    let parsed = serde_json::from_str::<Value>(arguments)
        .unwrap_or_else(|_| Value::String(arguments.to_owned()));
    let value = parsed
        .get("input")
        .filter(|_| parsed.is_object())
        .unwrap_or(&parsed);
    if let Value::String(value) = value {
        value.clone()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| arguments.to_owned())
    }
}

/// Project a Responses request into Anthropic Messages.
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

fn output_message(id: &str, text: &str) -> Value {
    serde_json::json!({
        "id": id, "type": "message", "status": "completed", "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}]
    })
}

/// Project one complete Anthropic Messages response into Responses JSON.
///
/// Stable generated IDs are supplied by the caller so concurrent turns cannot
/// share a fixed fixture identifier and tests remain deterministic.
pub fn response_from_anthropic(
    value: &Value,
    requested_model: &str,
    custom_names: &[&str],
    ids: &mut AnthropicIds,
) -> Result<Value, AnthropicError> {
    let Some(root) = object(value) else {
        return Err(upstream_error(
            "Anthropic upstream returned invalid content",
        ));
    };
    let is_truthy = |value: &Value| match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    };
    if root.get("error").is_some_and(is_truthy) {
        return Err(upstream_error("Anthropic upstream returned an error"));
    }
    let mut output = Vec::new();
    let mut combined_text = String::new();
    let custom_names = custom_names
        .iter()
        .map(|name| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    let content = root
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| upstream_error("Anthropic upstream returned invalid content"))?;
    let mut call_ids = BTreeSet::new();
    for block in content {
        let Some(block) = object(block) else {
            return Err(upstream_error(
                "Anthropic upstream returned invalid content",
            ));
        };
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| upstream_error("Anthropic upstream returned invalid content"))?;
                if !text.is_empty() {
                    combined_text.push_str(text);
                    output.push(output_message(&ids.next_message(), text));
                }
            }
            Some("tool_use") => {
                let raw_id = required_upstream_string(
                    block.get("id"),
                    "Anthropic upstream returned an invalid tool call",
                )?;
                let name = required_upstream_string(
                    block.get("name"),
                    "Anthropic upstream returned an invalid tool call",
                )?;
                if !call_ids.insert(raw_id.to_owned()) {
                    return Err(upstream_error(
                        "Anthropic upstream returned a duplicate tool call ID",
                    ));
                }
                let arguments = tool_arguments(block.get("input"))?;
                if custom_names.contains(name) {
                    output.push(serde_json::json!({
                        "id": custom_tool_id(raw_id), "type": "custom_tool_call",
                        "status": "completed", "call_id": raw_id, "name": name,
                        "input": custom_tool_input(&arguments)
                    }));
                } else {
                    output.push(serde_json::json!({
                        "id": raw_id, "type": "function_call", "status": "completed",
                        "call_id": raw_id, "name": name, "arguments": arguments
                    }));
                }
            }
            _ => {
                return Err(upstream_error(
                    "Anthropic upstream returned unsupported content",
                ));
            }
        }
    }
    let reason = incomplete_reason(root.get("stop_reason"))?;
    let mut response = serde_json::json!({
        "id": ids.response.clone(), "object": "response",
        "status": if reason.is_some() { "incomplete" } else { "completed" },
        "model": requested_model, "output": output, "output_text": combined_text
    });
    if let Some(reason) = reason {
        response["incomplete_details"] = serde_json::json!({"reason": reason});
    }
    if let Some(usage) = root.get("usage").filter(|usage| usage.is_object()) {
        response["usage"] = anthropic_usage(usage);
    }
    Ok(response)
}

#[derive(Debug, Clone)]
pub struct AnthropicIds {
    response: String,
    message: String,
    message_offset: usize,
}

impl AnthropicIds {
    pub fn new(response: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            message: message.into(),
            message_offset: 0,
        }
    }

    fn next_message(&mut self) -> String {
        let result = if self.message_offset == 0 {
            self.message.clone()
        } else {
            format!("{}_{}", self.message, self.message_offset)
        };
        self.message_offset += 1;
        result
    }
}

const MAX_ANTHROPIC_STREAM_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ANTHROPIC_TOOL_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
enum AnthropicBlockKind {
    Text {
        id: String,
        parts: Vec<String>,
        explicit: bool,
    },
    Tool {
        id: String,
        call_id: String,
        name: String,
        custom: bool,
        initial_input: Map<String, Value>,
        json_parts: Vec<String>,
        json_bytes: usize,
    },
    SuppressedReasoning,
}

#[derive(Debug, Clone)]
struct AnthropicBlockState {
    output_index: Option<usize>,
    closed: bool,
    item: Option<Value>,
    kind: AnthropicBlockKind,
}

/// Pure Anthropic Messages stream state machine.
///
/// The transport supplies already parsed JSON events. This type owns event
/// ordering, block validation, usage accumulation and terminal truth.
#[derive(Clone)]
pub struct AnthropicStream {
    ids: AnthropicIds,
    sequence: u64,
    response: Map<String, Value>,
    blocks: BTreeMap<usize, AnthropicBlockState>,
    ordered_blocks: Vec<usize>,
    tool_call_ids: BTreeSet<String>,
    custom_names: BTreeSet<String>,
    all_text: String,
    text_bytes: usize,
    stop_reason: Option<Value>,
    usage: Map<String, Value>,
    saw_message_stop: bool,
    started: bool,
    finished: bool,
}

impl std::fmt::Debug for AnthropicStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicStream")
            .field("response_id", &self.ids.response)
            .field("blocks", &self.blocks.len())
            .field("saw_message_stop", &self.saw_message_stop)
            .field("started", &self.started)
            .field("finished", &self.finished)
            .finish()
    }
}

impl AnthropicStream {
    pub fn new(
        response_model: impl Into<String>,
        ids: AnthropicIds,
        custom_names: &[&str],
    ) -> Self {
        let response = json!({
            "id": ids.response.clone(),
            "object": "response",
            "status": "in_progress",
            "model": response_model.into(),
            "output": []
        });
        Self {
            ids,
            sequence: 0,
            response: response.as_object().expect("literal response").clone(),
            blocks: BTreeMap::new(),
            ordered_blocks: Vec::new(),
            tool_call_ids: BTreeSet::new(),
            custom_names: custom_names.iter().map(|name| (*name).to_owned()).collect(),
            all_text: String::new(),
            text_bytes: 0,
            stop_reason: None,
            usage: Map::new(),
            saw_message_stop: false,
            started: false,
            finished: false,
        }
    }

    pub fn start_event(&mut self) -> Result<crate::StreamEvent, AnthropicError> {
        if self.started || self.finished {
            return Err(upstream_error("Anthropic stream already started"));
        }
        self.started = true;
        let response = Value::Object(self.response.clone());
        Ok(self.event(
            "response.created",
            json!({"type": "response.created", "response": response}),
        ))
    }

    pub fn push(&mut self, event: &Value) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        if !self.started || self.finished {
            return Err(upstream_error("Anthropic stream is not accepting events"));
        }
        let event = object(event)
            .ok_or_else(|| upstream_error("Anthropic upstream returned an invalid stream event"))?;
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "error" {
            return Err(upstream_error("Anthropic upstream returned an error event"));
        }
        match event_type {
            "message_start" => {
                if let Some(value) = event
                    .get("message")
                    .and_then(object)
                    .and_then(|message| message.get("usage"))
                    .and_then(object)
                {
                    self.usage.extend(value.clone());
                }
                Ok(Vec::new())
            }
            "message_delta" => {
                if let Some(value) = event.get("usage").and_then(object) {
                    self.usage.extend(value.clone());
                }
                if let Some(reason) = event
                    .get("delta")
                    .and_then(object)
                    .and_then(|delta| delta.get("stop_reason"))
                    .filter(|reason| python_truthy(Some(reason)))
                {
                    self.stop_reason = Some(reason.clone());
                }
                Ok(Vec::new())
            }
            "message_stop" => {
                self.saw_message_stop = true;
                Ok(Vec::new())
            }
            "content_block_start" => self.start_block(event),
            "content_block_delta" => self.push_delta(event),
            "content_block_stop" => self.stop_block(event),
            _ => Ok(Vec::new()),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        if !self.started || self.finished {
            return Err(stream_error("Anthropic stream cannot finish twice"));
        }
        if !self.saw_message_stop {
            return Err(stream_error("Anthropic upstream ended before message_stop"));
        }
        let incomplete = incomplete_reason(self.stop_reason.as_ref())?;
        let mut events = Vec::new();
        for raw_index in self.ordered_blocks.clone() {
            let (explicit, closed) = match &self.blocks[&raw_index].kind {
                AnthropicBlockKind::Text { explicit, .. } => {
                    (*explicit, self.blocks[&raw_index].closed)
                }
                _ => (true, self.blocks[&raw_index].closed),
            };
            if closed {
                continue;
            }
            if explicit {
                return Err(stream_error(
                    "Anthropic upstream ended with an unfinished content block",
                ));
            }
            events.extend(self.close_text(raw_index)?);
        }
        let output = self
            .ordered_blocks
            .iter()
            .filter_map(|index| self.blocks[index].item.clone())
            .collect::<Vec<_>>();
        if output.is_empty() {
            return Err(stream_error(
                "upstream returned an empty Anthropic Messages response",
            ));
        }
        self.finished = true;
        self.response.insert(
            "status".to_owned(),
            Value::String(
                if incomplete.is_some() {
                    "incomplete"
                } else {
                    "completed"
                }
                .to_owned(),
            ),
        );
        self.response
            .insert("output".to_owned(), Value::Array(output));
        self.response.insert(
            "output_text".to_owned(),
            Value::String(self.all_text.clone()),
        );
        if !self.usage.is_empty() {
            self.response.insert(
                "usage".to_owned(),
                anthropic_usage(&Value::Object(self.usage.clone())),
            );
        }
        if let Some(reason) = incomplete {
            self.response
                .insert("incomplete_details".to_owned(), json!({"reason": reason}));
        }
        let terminal = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let response = Value::Object(self.response.clone());
        events.push(self.event(terminal, json!({"type": terminal, "response": response})));
        Ok(events)
    }

    fn raw_index(event: &Map<String, Value>, default: usize) -> Result<usize, AnthropicError> {
        match event.get("index") {
            None => Ok(default),
            Some(Value::Number(number)) => number
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    upstream_error("Anthropic upstream returned an invalid content index")
                }),
            Some(_) => Err(upstream_error(
                "Anthropic upstream returned an invalid content index",
            )),
        }
    }

    fn start_block(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, self.blocks.len())?;
        if self.blocks.contains_key(&raw_index) {
            return Err(upstream_error(
                "Anthropic upstream repeated a content block",
            ));
        }
        let block = event.get("content_block").and_then(object).ok_or_else(|| {
            upstream_error("Anthropic upstream returned an invalid content block")
        })?;
        let output_index = self.ordered_blocks.len();
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let initial = match block.get("text") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned invalid message content",
                        ));
                    }
                };
                let id = self.ids.next_message();
                self.blocks.insert(
                    raw_index,
                    AnthropicBlockState {
                        output_index: Some(output_index),
                        closed: false,
                        item: None,
                        kind: AnthropicBlockKind::Text {
                            id: id.clone(),
                            parts: Vec::new(),
                            explicit: true,
                        },
                    },
                );
                self.ordered_blocks.push(raw_index);
                let mut events = vec![
                    self.event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added", "output_index": output_index,
                            "item": {"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}
                        }),
                    ),
                    self.event(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added", "item_id": id,
                            "output_index": output_index, "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        }),
                    ),
                ];
                if let Some(initial) = initial.filter(|text| !text.is_empty()) {
                    events.extend(self.push_text(raw_index, &initial)?);
                }
                Ok(events)
            }
            "tool_use" => {
                let raw_id = required_upstream_string(
                    block.get("id"),
                    "Anthropic upstream returned an invalid tool call",
                )?;
                let name = required_upstream_string(
                    block.get("name"),
                    "Anthropic upstream returned an invalid tool call",
                )?;
                let initial_input = match block.get("input") {
                    None => Map::new(),
                    Some(Value::Object(input)) => input.clone(),
                    Some(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned an invalid tool call",
                        ));
                    }
                };
                if !self.tool_call_ids.insert(raw_id.to_owned()) {
                    return Err(upstream_error(
                        "Anthropic upstream returned a duplicate tool call ID",
                    ));
                }
                let custom = self.custom_names.contains(name);
                let id = if custom {
                    custom_tool_id(raw_id)
                } else {
                    raw_id.to_owned()
                };
                self.blocks.insert(
                    raw_index,
                    AnthropicBlockState {
                        output_index: Some(output_index),
                        closed: false,
                        item: None,
                        kind: AnthropicBlockKind::Tool {
                            id: id.clone(),
                            call_id: raw_id.to_owned(),
                            name: name.to_owned(),
                            custom,
                            initial_input,
                            json_parts: Vec::new(),
                            json_bytes: 0,
                        },
                    },
                );
                self.ordered_blocks.push(raw_index);
                let mut item = json!({
                    "id": id, "type": if custom { "custom_tool_call" } else { "function_call" },
                    "status": "in_progress", "call_id": raw_id, "name": name
                });
                item[if custom { "input" } else { "arguments" }] = Value::String(String::new());
                Ok(vec![self.event(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "output_index": output_index, "item": item}),
                )])
            }
            "thinking" | "redacted_thinking" => {
                self.blocks.insert(
                    raw_index,
                    AnthropicBlockState {
                        output_index: None,
                        closed: false,
                        item: None,
                        kind: AnthropicBlockKind::SuppressedReasoning,
                    },
                );
                Ok(Vec::new())
            }
            _ => Err(upstream_error(
                "Anthropic upstream returned an unsupported content block",
            )),
        }
    }

    fn push_delta(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, 0)?;
        let delta = event.get("delta").and_then(object).ok_or_else(|| {
            upstream_error("Anthropic upstream returned an invalid content delta")
        })?;
        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
        if !self.blocks.contains_key(&raw_index) && delta_type == "text_delta" {
            let output_index = self.ordered_blocks.len();
            let id = self.ids.next_message();
            self.blocks.insert(
                raw_index,
                AnthropicBlockState {
                    output_index: Some(output_index),
                    closed: false,
                    item: None,
                    kind: AnthropicBlockKind::Text {
                        id: id.clone(),
                        parts: Vec::new(),
                        explicit: false,
                    },
                },
            );
            self.ordered_blocks.push(raw_index);
            let mut events = vec![
                self.event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added", "output_index": output_index,
                        "item": {"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}
                    }),
                ),
                self.event(
                    "response.content_part.added",
                    json!({
                        "type": "response.content_part.added", "item_id": id,
                        "output_index": output_index, "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []}
                    }),
                ),
            ];
            if let Some(piece) = delta.get("text").and_then(Value::as_str) {
                if !piece.is_empty() {
                    events.extend(self.push_text(raw_index, piece)?);
                }
                return Ok(events);
            }
            return Err(upstream_error(
                "Anthropic upstream returned invalid message content",
            ));
        }
        let state = self.blocks.get(&raw_index).ok_or_else(|| {
            upstream_error("Anthropic upstream returned a delta for an unknown content block")
        })?;
        if state.closed {
            return Err(upstream_error(
                "Anthropic upstream returned a delta for an unknown content block",
            ));
        }
        if matches!(state.kind, AnthropicBlockKind::SuppressedReasoning) {
            return Ok(Vec::new());
        }
        match (&state.kind, delta_type) {
            (AnthropicBlockKind::Text { .. }, "text_delta") => {
                let piece = delta.get("text").and_then(Value::as_str).ok_or_else(|| {
                    upstream_error("Anthropic upstream returned invalid message content")
                })?;
                self.push_text(raw_index, piece)
            }
            (AnthropicBlockKind::Tool { .. }, "input_json_delta") => {
                let piece = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        upstream_error("Anthropic upstream returned invalid tool input")
                    })?;
                let bytes = piece.len();
                let (custom, item_id, output_index) = {
                    let state = self.blocks.get_mut(&raw_index).expect("checked block");
                    let AnthropicBlockKind::Tool {
                        custom,
                        id,
                        json_parts,
                        json_bytes,
                        ..
                    } = &mut state.kind
                    else {
                        unreachable!()
                    };
                    if json_bytes.saturating_add(bytes) > MAX_ANTHROPIC_TOOL_INPUT_BYTES {
                        return Err(upstream_error("Anthropic upstream tool input is too large"));
                    }
                    *json_bytes += bytes;
                    json_parts.push(piece.to_owned());
                    (
                        *custom,
                        id.clone(),
                        state.output_index.expect("tool output index"),
                    )
                };
                if piece.is_empty() || custom {
                    Ok(Vec::new())
                } else {
                    Ok(vec![self.event(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta", "item_id": item_id,
                            "output_index": output_index, "delta": piece
                        }),
                    )])
                }
            }
            _ => Err(upstream_error(
                "Anthropic upstream returned a mismatched content delta",
            )),
        }
    }

    fn push_text(
        &mut self,
        raw_index: usize,
        piece: &str,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        if piece.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = piece.len();
        if self.text_bytes.saturating_add(bytes) > MAX_ANTHROPIC_STREAM_TEXT_BYTES {
            return Err(upstream_error("upstream streamed text is too large"));
        }
        self.text_bytes += bytes;
        self.all_text.push_str(piece);
        let (id, output_index) = {
            let state = self.blocks.get_mut(&raw_index).expect("text block exists");
            let AnthropicBlockKind::Text { id, parts, .. } = &mut state.kind else {
                unreachable!()
            };
            parts.push(piece.to_owned());
            (id.clone(), state.output_index.expect("text output index"))
        };
        Ok(vec![self.event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta", "item_id": id,
                "output_index": output_index, "content_index": 0, "delta": piece
            }),
        )])
    }

    fn stop_block(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, 0)?;
        let state = self
            .blocks
            .get(&raw_index)
            .ok_or_else(|| upstream_error("Anthropic upstream stopped an unknown content block"))?;
        if state.closed {
            return Err(upstream_error(
                "Anthropic upstream stopped an unknown content block",
            ));
        }
        if matches!(state.kind, AnthropicBlockKind::SuppressedReasoning) {
            self.blocks
                .get_mut(&raw_index)
                .expect("checked block")
                .closed = true;
            return Ok(Vec::new());
        }
        if matches!(state.kind, AnthropicBlockKind::Text { .. }) {
            return self.close_text(raw_index);
        }
        self.close_tool(raw_index)
    }

    fn close_text(&mut self, raw_index: usize) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let (id, output_index, text) = {
            let state = self.blocks.get_mut(&raw_index).expect("text block exists");
            let AnthropicBlockKind::Text { id, parts, .. } = &state.kind else {
                unreachable!()
            };
            let id = id.clone();
            let text = parts.concat();
            let output_index = state.output_index.expect("text output index");
            let item = output_message(&id, &text);
            state.item = Some(item);
            state.closed = true;
            (id, output_index, text)
        };
        let item = self.blocks[&raw_index].item.clone().expect("text item");
        Ok(vec![
            self.event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done", "item_id": id,
                    "output_index": output_index, "content_index": 0, "text": text
                }),
            ),
            self.event(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done", "item_id": id,
                    "output_index": output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []}
                }),
            ),
            self.event(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": output_index, "item": item}),
            ),
        ])
    }

    fn close_tool(&mut self, raw_index: usize) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let (id, call_id, name, custom, output_index, tool_input) = {
            let state = self.blocks.get(&raw_index).expect("tool block exists");
            let AnthropicBlockKind::Tool {
                id,
                call_id,
                name,
                custom,
                initial_input,
                json_parts,
                ..
            } = &state.kind
            else {
                unreachable!()
            };
            let input = if json_parts.is_empty() {
                Value::Object(initial_input.clone())
            } else {
                match serde_json::from_str::<Value>(&json_parts.concat()) {
                    Ok(Value::Object(input)) => Value::Object(input),
                    Ok(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned invalid tool input",
                        ));
                    }
                    Err(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned malformed tool input",
                        ));
                    }
                }
            };
            (
                id.clone(),
                call_id.clone(),
                name.clone(),
                *custom,
                state.output_index.expect("tool output index"),
                input,
            )
        };
        let arguments = tool_arguments(Some(&tool_input))?;
        let projected = if custom {
            custom_tool_input(&arguments)
        } else {
            arguments.clone()
        };
        let mut item = json!({
            "id": id, "type": if custom { "custom_tool_call" } else { "function_call" },
            "status": "completed", "call_id": call_id, "name": name
        });
        item[if custom { "input" } else { "arguments" }] = Value::String(projected.clone());
        {
            let state = self.blocks.get_mut(&raw_index).expect("tool block exists");
            state.item = Some(item.clone());
            state.closed = true;
        }
        let mut events = Vec::new();
        if custom {
            events.push(self.event(
                "response.custom_tool_call_input.delta",
                json!({
                    "type": "response.custom_tool_call_input.delta", "item_id": id,
                    "output_index": output_index, "delta": projected
                }),
            ));
        } else {
            events.push(self.event(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done", "item_id": id,
                    "output_index": output_index, "arguments": arguments
                }),
            ));
        }
        events.push(self.event(
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": output_index, "item": item}),
        ));
        Ok(events)
    }

    fn event(&mut self, event: &'static str, mut value: Value) -> crate::StreamEvent {
        self.sequence += 1;
        if let Some(value) = value.as_object_mut() {
            value.insert("sequence_number".to_owned(), Value::from(self.sequence));
        }
        crate::StreamEvent { event, value }
    }
}
