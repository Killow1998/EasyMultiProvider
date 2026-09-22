//! Responses request projection onto the OpenAI Chat Completions dialect.

use super::{ProtocolError, request_error};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

const COMPACTION_PREFIX: &str = "emp1:";
const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started this task and produced a continuation summary. Use it to continue without repeating completed work:";
const CUSTOM_TOOL_PARAMETERS: &str = r#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"],"additionalProperties":false}"#;

fn object(value: &Value) -> Option<&Map<String, Value>> {
    value.as_object()
}

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

fn chat_content(value: Option<&Value>) -> Result<Value, ProtocolError> {
    match value {
        Some(Value::String(value)) => return Ok(Value::String(value.clone())),
        Some(Value::Array(_)) => {}
        _ => {
            return Err(request_error(
                "request projection failed: invalid message content",
            ));
        }
    }
    let parts = value
        .and_then(Value::as_array)
        .expect("array checked above");
    let mut projected = Vec::with_capacity(parts.len());
    let mut has_nontext = false;
    for part in parts {
        if let Value::String(text) = part {
            projected.push(serde_json::json!({"type": "text", "text": text}));
            continue;
        }
        let Some(part) = object(part) else {
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

fn custom_tool_arguments(value: Option<&Value>) -> Result<String, ProtocolError> {
    let input = match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => serde_json::to_string(value)
            .map_err(|_| request_error("request projection failed: invalid custom tool input"))?,
        None => String::new(),
    };
    serde_json::to_string(&serde_json::json!({"input": input}))
        .map_err(|_| request_error("request projection failed: invalid custom tool input"))
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

fn standalone_tool_output(item: &Map<String, Value>) -> Result<Option<String>, ProtocolError> {
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
    let output = request_text(
        item.get("output").or(Some(&Value::String(String::new()))),
        "request projection failed: invalid standalone tool output",
        "request projection failed: unsupported standalone tool output",
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
    let mut call_ids = BTreeSet::new();
    let mut output_ids = BTreeSet::new();
    for raw in source {
        let Some(item) = object(raw) else {
            return Err(request_error(
                "request projection failed: invalid input item",
            ));
        };
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message");
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
                    custom_tool_arguments(item.get("input"))?
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
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": request_text(
                        item.get("output").or(Some(&empty_output)),
                        "request projection failed: invalid tool output",
                        "request projection failed: unsupported tool output",
                    )?,
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
    Ok(messages)
}

#[derive(Clone)]
struct RawTool {
    value: Map<String, Value>,
    namespace: Vec<String>,
}

fn visit_tools(
    value: Option<&Value>,
    namespace: &[String],
    result: &mut Vec<RawTool>,
) -> Result<(), ProtocolError> {
    let Some(Value::Array(items)) = value else {
        return Ok(());
    };
    for item in items {
        let Some(item) = object(item) else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) == Some("namespace") {
            let name = required_string(
                item.get("name"),
                "request projection failed: invalid tool namespace",
            )?;
            let mut child = namespace.to_vec();
            child.push(name.to_owned());
            visit_tools(item.get("tools"), &child, result)?;
        } else {
            result.push(RawTool {
                value: item.clone(),
                namespace: namespace.to_vec(),
            });
        }
    }
    Ok(())
}

fn raw_tools(body: &Map<String, Value>) -> Result<Vec<RawTool>, ProtocolError> {
    let mut result = Vec::new();
    visit_tools(body.get("tools"), &[], &mut result)?;
    let source = body.get("input");
    match source {
        Some(Value::Object(item)) => {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                visit_tools(item.get("tools"), &[], &mut result)?;
            }
        }
        Some(Value::Array(items)) => {
            for item in items.iter().filter_map(Value::as_object) {
                if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                    visit_tools(item.get("tools"), &[], &mut result)?;
                }
            }
        }
        _ => {}
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

fn tools(body: &Map<String, Value>) -> Result<Vec<Value>, ProtocolError> {
    let raw = raw_tools(body)?;
    let custom_parameters: Value =
        serde_json::from_str(CUSTOM_TOOL_PARAMETERS).expect("static custom tool schema");
    let mut seen: BTreeMap<String, (Vec<String>, String, Value)> = BTreeMap::new();
    let mut result = Vec::new();
    for item in raw {
        let tool_type = item
            .value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(tool_type, "function" | "custom") {
            return Err(request_error(
                "request projection failed: unsupported tool definition",
            ));
        }
        let function = item
            .value
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(&item.value);
        let name = required_string(
            function.get("name"),
            "request projection failed: invalid tool definition",
        )?;
        let parameters = if tool_type == "custom" {
            custom_parameters.clone()
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
        let identity = (
            item.namespace,
            tool_type.to_owned(),
            Value::Object(function.clone()),
        );
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
            "type": "function",
            "function": {
                "name": name,
                "description": python_description(function.get("description")),
                "parameters": parameters,
            }
        }));
    }
    Ok(result)
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

fn tool_choice(value: &Value) -> Result<Value, ProtocolError> {
    if let Some(value @ ("auto" | "none" | "required")) = value.as_str() {
        return Ok(Value::String(value.to_owned()));
    }
    if let Some(value) = object(value)
        && matches!(
            value.get("type").and_then(Value::as_str),
            Some("function" | "custom")
        )
        && let Some(name) = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
    {
        return Ok(serde_json::json!({
            "type": "function", "function": {"name": name}
        }));
    }
    Err(request_error(
        "request projection failed: unsupported tool choice",
    ))
}

fn response_format(body: &Map<String, Value>) -> Result<Option<Value>, ProtocolError> {
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
    let body = object(body)
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
        .and_then(Value::as_str)
    {
        payload["reasoning_effort"] = Value::String(effort.to_owned());
    }
    if let Some(format) = response_format(body)? {
        payload["response_format"] = format;
    }
    Ok(payload)
}
