//! Stateless Responses input, media, instructions and portable checkpoints.
use super::{
    COMPACTION_PREFIX, COMPACTION_SUMMARY_PREFIX, PortableProjectionError, error, python_string,
};
use base64::Engine as _;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

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
        .filter_map(|part| {
            part.as_object()
                .map(|part| python_string(part.get("type"), "unknown"))
        })
        .collect()
}

fn text_part_text(part: &Map<String, Value>) -> Option<String> {
    let text = part.get("text").and_then(Value::as_str)?;
    matches!(
        part.get("type").and_then(Value::as_str),
        Some("input_text" | "output_text" | "text")
    )
    .then(|| text.to_owned())
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
                let Some(part) = part.as_object() else {
                    return Err(PortableProjectionError::new(
                        index,
                        "message",
                        part_types,
                        "invalid_content_part",
                    ));
                };
                let kind = part.get("type").and_then(Value::as_str);
                if let Some(text) = text_part_text(part) {
                    projected.push(json!({"type": kind, "text": text}));
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
                if kind == Some("input_audio")
                    && let Some(audio) = crate::chat_request::input_audio(part)
                {
                    projected.push(json!({"type": "input_audio", "input_audio": audio}));
                    continue;
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
                    Value::Object(part) if text_part_text(part).is_some() => {
                        text.push(text_part_text(part).unwrap());
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

pub(super) fn portable_input(
    source: Option<&Value>,
    preserve_reasoning_state: bool,
) -> Result<(Value, Vec<String>), PortableProjectionError> {
    let owned;
    let source = match source {
        None | Some(Value::Null) => return Ok((Value::Null, Vec::new())),
        Some(Value::String(value)) => return Ok((Value::String(value.clone()), Vec::new())),
        Some(Value::Object(value)) => {
            owned = vec![Value::Object(value.clone())];
            owned.as_slice()
        }
        Some(Value::Array(value)) => value.as_slice(),
        Some(_) => return Err(error(0, "input", "invalid_input")),
    };
    let mut projected = Vec::with_capacity(source.len());
    let mut instructions = Vec::new();
    let mut calls = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for (index, raw) in source.iter().enumerate() {
        let Some(item) = raw.as_object() else {
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
