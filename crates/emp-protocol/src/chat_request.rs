//! Responses request projection onto the OpenAI Chat Completions dialect.

use super::{ProtocolError, request_error};
use crate::tool_definitions::custom_tool_arguments;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

mod tools;
use tools::{tool_choice, tools};

const COMPACTION_PREFIX: &str = "emp1:";
const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started this task and produced a continuation summary. Use it to continue without repeating completed work:";

fn required_string<'a>(
    value: Option<&'a Value>,
    message: &'static str,
) -> Result<&'a str, ProtocolError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| request_error(message))
}

fn request_text(
    value: Option<&Value>,
    invalid: &'static str,
    unsupported: &'static str,
) -> Result<String, ProtocolError> {
    match value {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(Value::Array(parts)) => {
            let mut result = String::new();
            for part in parts {
                match part {
                    Value::String(text) => result.push_str(text),
                    Value::Object(part)
                        if matches!(
                            part.get("type").and_then(Value::as_str),
                            Some("input_text" | "output_text" | "text")
                        ) =>
                    {
                        let text = part
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| request_error(invalid))?;
                        result.push_str(text);
                    }
                    Value::Object(_) => return Err(request_error(unsupported)),
                    _ => return Err(request_error(unsupported)),
                }
            }
            Ok(result)
        }
        _ => Err(request_error(invalid)),
    }
}

/// Base64 audio with its container format, as both Responses and Chat Completions carry it.
pub(crate) fn input_audio(part: &Map<String, Value>) -> Option<Value> {
    let audio = part.get("input_audio").and_then(Value::as_object)?;
    let data = audio
        .get("data")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    let format = audio
        .get("format")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?;
    Some(serde_json::json!({"data": data, "format": format}))
}

fn chat_content(value: Option<&Value>) -> Result<Value, ProtocolError> {
    let parts = match value {
        Some(Value::String(value)) => return Ok(Value::String(value.clone())),
        Some(Value::Array(parts)) => parts,
        _ => {
            return Err(request_error(
                "request projection failed: invalid message content",
            ));
        }
    };
    let mut projected = Vec::with_capacity(parts.len());
    let mut has_nontext = false;
    for part in parts {
        if let Value::String(text) = part {
            projected.push(serde_json::json!({"type": "text", "text": text}));
            continue;
        }
        let Some(part) = part.as_object() else {
            return Err(request_error(
                "request projection failed: invalid message content",
            ));
        };
        match part.get("type").and_then(Value::as_str) {
            Some("refusal") => {
                let refusal = part
                    .get("refusal")
                    .and_then(Value::as_str)
                    .ok_or_else(|| request_error("request projection failed: invalid refusal"))?;
                projected.push(serde_json::json!({"type": "refusal", "refusal": refusal}));
                has_nontext = true;
            }
            Some("input_text" | "output_text" | "text") => {
                let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                    request_error("request projection failed: invalid message content")
                })?;
                projected.push(serde_json::json!({"type": "text", "text": text}));
            }
            Some("input_image" | "output_image") => {
                let image_url = match part.get("image_url") {
                    Some(Value::String(value)) => Some(value.as_str()),
                    Some(Value::Object(value)) => value.get("url").and_then(Value::as_str),
                    _ => None,
                }
                .filter(|value| !value.is_empty())
                .ok_or_else(|| request_error("request projection failed: invalid image"))?;
                let mut image = serde_json::json!({"url": image_url});
                if let Some(detail @ ("auto" | "low" | "high" | "original")) =
                    part.get("detail").and_then(Value::as_str)
                {
                    image["detail"] = Value::String(detail.to_owned());
                }
                projected.push(serde_json::json!({"type": "image_url", "image_url": image}));
                has_nontext = true;
            }
            Some("input_audio") => {
                let audio = input_audio(part)
                    .ok_or_else(|| request_error("request projection failed: invalid audio"))?;
                projected.push(serde_json::json!({"type": "input_audio", "input_audio": audio}));
                has_nontext = true;
            }
            _ => {
                return Err(request_error(
                    "request projection failed: unsupported message content",
                ));
            }
        }
    }
    if has_nontext {
        Ok(Value::Array(projected))
    } else {
        Ok(Value::String(
            projected
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect(),
        ))
    }
}

fn agent_message_text(item: &Map<String, Value>) -> Result<String, ProtocolError> {
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

fn decode_compaction(item: &Map<String, Value>) -> Option<String> {
    let encoded = item
        .get("encrypted_content")
        .and_then(Value::as_str)?
        .strip_prefix(COMPACTION_PREFIX)?;
    let decoded = URL_SAFE.decode(encoded).ok()?;
    String::from_utf8(decoded).ok()
}

fn request_tool_arguments(value: Option<&Value>) -> Result<String, ProtocolError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| request_error("request projection failed: invalid tool arguments"))?;
    if !matches!(serde_json::from_str::<Value>(value), Ok(Value::Object(_))) {
        return Err(request_error(
            "request projection failed: invalid tool arguments",
        ));
    }
    Ok(value.to_owned())
}

fn flush_calls(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    messages.push(serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": std::mem::take(pending),
    }));
}

fn flush_tool_media(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if !pending.is_empty() {
        messages.push(serde_json::json!({
            "role": "user", "content": std::mem::take(pending),
        }));
    }
}

fn standalone_tool_output(item: &Map<String, Value>) -> Result<Option<Value>, ProtocolError> {
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
    let output = chat_content(item.get("output").or(Some(&Value::String(String::new()))))?;
    let label = if namespace.is_empty() {
        name.to_owned()
    } else {
        format!("{namespace}/{name}")
    };
    let header = format!("Standalone tool output from {label}:\n");
    Ok(Some(match output {
        Value::String(text) => Value::String(format!("{header}{text}")),
        Value::Array(mut parts) => {
            parts.insert(0, serde_json::json!({"type": "text", "text": header}));
            Value::Array(parts)
        }
        _ => unreachable!("chat content is text or content parts"),
    }))
}

fn messages(body: &Map<String, Value>) -> Result<Vec<Value>, ProtocolError> {
    let mut messages = Vec::new();
    if let Some(instructions) = body.get("instructions") {
        messages.push(serde_json::json!({
            "role": "system",
            "content": request_text(
                Some(instructions),
                "request projection failed: invalid instructions",
                "request projection failed: unsupported instructions",
            )?
        }));
    }
    let empty_input = Value::String(String::new());
    let source = body.get("input").unwrap_or(&empty_input);
    if let Value::String(text) = source {
        messages.push(serde_json::json!({"role": "user", "content": text}));
        return Ok(messages);
    }
    let source = match source {
        Value::Object(_) => vec![source],
        Value::Array(source) => source.iter().collect::<Vec<_>>(),
        _ => return Err(request_error("request projection failed: invalid input")),
    };
    let mut pending_calls = Vec::new();
    let mut pending_tool_media = Vec::new();
    let mut call_ids = BTreeSet::new();
    let mut output_ids = BTreeSet::new();
    for raw in source {
        let Some(item) = raw.as_object() else {
            return Err(request_error(
                "request projection failed: invalid input item",
            ));
        };
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
        if !matches!(
            item_type,
            "function_call_output" | "custom_tool_call_output" | "reasoning" | "additional_tools"
        ) {
            flush_tool_media(&mut messages, &mut pending_tool_media);
        }
        match item_type {
            "compaction" => {
                flush_calls(&mut messages, &mut pending_calls);
                let summary = decode_compaction(item).ok_or_else(|| {
                    request_error("request projection failed: history projection incomplete")
                })?;
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!("{COMPACTION_SUMMARY_PREFIX}\n\n{summary}"),
                }));
            }
            "message" => {
                flush_calls(&mut messages, &mut pending_calls);
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if !matches!(role, "user" | "assistant" | "system" | "developer") {
                    return Err(request_error(
                        "request projection failed: unsupported message role",
                    ));
                }
                messages.push(serde_json::json!({
                    "role": role,
                    "content": chat_content(item.get("content").or(Some(&empty_input)))?,
                }));
            }
            "agent_message" => {
                flush_calls(&mut messages, &mut pending_calls);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": agent_message_text(item)?,
                }));
            }
            "function_call" | "custom_tool_call" => {
                let call_id = required_string(
                    item.get("call_id")
                        .filter(|value| value.as_str().is_some_and(|v| !v.is_empty()))
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
                let arguments = if item_type == "custom_tool_call" {
                    custom_tool_arguments(item.get("input")).to_string()
                } else {
                    let default_arguments = Value::String("{}".to_owned());
                    request_tool_arguments(item.get("arguments").or(Some(&default_arguments)))?
                };
                let mut call = serde_json::json!({
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments},
                });
                if let Some(extra) = item.get("extra_content").filter(|value| value.is_object()) {
                    call["extra_content"] = extra.clone();
                }
                pending_calls.push(call);
            }
            "function_call_output" | "custom_tool_call_output" => {
                flush_calls(&mut messages, &mut pending_calls);
                if let Some(standalone) = standalone_tool_output(item)? {
                    flush_tool_media(&mut messages, &mut pending_tool_media);
                    messages.push(serde_json::json!({"role": "user", "content": standalone}));
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
                let content = match chat_content(item.get("output").or(Some(&empty_output)))? {
                    text @ Value::String(_) if pending_tool_media.is_empty() => text,
                    content => {
                        // Chat tool messages accept text only. Attach media after all paired results.
                        pending_tool_media.push(serde_json::json!({
                            "type": "text", "text": format!("Tool result for {call_id}:\n"),
                        }));
                        if let Value::Array(parts) = content {
                            pending_tool_media.extend(parts);
                        } else {
                            pending_tool_media
                                .push(serde_json::json!({"type": "text", "text": content}));
                        }
                        Value::String(
                            "The complete tool result follows after the tool responses.".to_owned(),
                        )
                    }
                };
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
            }
            "reasoning" | "additional_tools" => {}
            _ => {
                flush_calls(&mut messages, &mut pending_calls);
                return Err(request_error(
                    "request projection failed: unsupported input item",
                ));
            }
        }
    }
    flush_calls(&mut messages, &mut pending_calls);
    flush_tool_media(&mut messages, &mut pending_tool_media);
    Ok(messages)
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn response_format(body: &Map<String, Value>) -> Result<Option<Value>, ProtocolError> {
    let Some(text) = body.get("text") else {
        return Ok(None);
    };
    let text = text
        .as_object()
        .ok_or_else(|| request_error("request projection failed: invalid text controls"))?;
    let Some(format) = text.get("format") else {
        return Ok(None);
    };
    let format = format
        .as_object()
        .filter(|format| format.get("type").and_then(Value::as_str) == Some("json_schema"))
        .ok_or_else(|| {
            request_error("request projection failed: unsupported structured output format")
        })?;
    let name = format
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
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
    let (Some(name), Some(schema)) = (name, schema) else {
        return Err(request_error(
            "request projection failed: invalid structured output format",
        ));
    };
    let mut result = serde_json::json!({
        "type": "json_schema",
        "json_schema": {"name": name, "schema": schema.clone()},
    });
    if let Some(strict) = strict {
        result["json_schema"]["strict"] = Value::Bool(strict);
    }
    Ok(Some(result))
}

/// Translate a canonical Responses request into Chat Completions without
/// dropping visible history, images, collaboration messages or tool pairing.
pub fn responses_to_chat(body: &Value, upstream_model: &str) -> Result<Value, ProtocolError> {
    let body = body
        .as_object()
        .ok_or_else(|| request_error("request projection failed: body must be an object"))?;
    let mut payload = serde_json::json!({
        "model": upstream_model,
        "messages": messages(body)?,
        "stream": python_truthy(body.get("stream")),
    });
    let projected_tools = tools(body)?;
    if !projected_tools.is_empty() {
        payload["tools"] = Value::Array(projected_tools);
    }
    if let Some(value) = body.get("tool_choice") {
        payload["tool_choice"] = tool_choice(value)?;
    }
    if let Some(value) = body.get("parallel_tool_calls") {
        if !value.is_boolean() {
            return Err(request_error(
                "request projection failed: parallel tool calls must be boolean",
            ));
        }
        payload["parallel_tool_calls"] = value.clone();
    }
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop", "stop"),
        ("max_output_tokens", "max_tokens"),
    ] {
        if let Some(value) = body.get(source) {
            payload[target] = value.clone();
        }
    }
    if let Some(effort) = body
        .get("reasoning")
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
    {
        if effort.is_number() {
            return Err(request_error(
                "request projection failed: numeric reasoning effort is unsupported by Chat Completions",
            ));
        }
        if let Some(effort) = effort.as_str() {
            payload["reasoning_effort"] = Value::String(effort.to_owned());
        }
    }
    if let Some(format) = response_format(body)? {
        payload["response_format"] = format;
    }
    Ok(payload)
}

#[cfg(test)]
mod audio_tests {
    use super::responses_to_chat;
    use serde_json::json;

    #[test]
    fn audio_input_reaches_chat_completions() {
        let body = json!({"model":"demo","input":[{"role":"user","content":[
            {"type":"input_text","text":"What do you hear?"},
            {"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}}
        ]}]});
        let chat = responses_to_chat(&body, "demo").expect("projected");
        let content = &chat["messages"].as_array().unwrap().last().unwrap()["content"];
        assert_eq!(
            content[1],
            json!({"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}})
        );
    }
}
