//! Anthropic message content, image data and paired tool history.
use super::tools::{custom_tool_arguments, request_tool_arguments};
use super::{
    ANTHROPIC_IMAGE_MEDIA_TYPES, AnthropicError, COMPACTION_PREFIX, COMPACTION_SUMMARY_PREFIX,
    OMITTED_INPUT_TYPES, TEXT_PART_TYPES, history_error, request_error, required_string,
};
use base64::Engine as _;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

pub(super) fn request_text(value: Option<&Value>, field: &str) -> Result<String, AnthropicError> {
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
        let Some(part) = part.as_object() else {
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
        let Some(part) = part.as_object() else {
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
        if TEXT_PART_TYPES.contains(&part_type.unwrap_or("")) {
            let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                request_error("request projection failed: invalid Anthropic content")
            })?;
            result.push(serde_json::json!({"type": "text", "text": text}));
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
pub(super) fn messages(body: &Map<String, Value>) -> Result<Vec<Value>, AnthropicError> {
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
        let Some(item) = item.as_object() else {
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
        let Some(item) = raw.as_object() else {
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
