//! Complete Anthropic response projection and its usage/tool helpers.

use super::*;

fn token_count(value: Option<&Value>) -> Option<u64> {
    match value {
        Some(Value::Number(number)) if number.is_u64() => {
            number.as_u64().filter(|value| *value <= 10_000_000)
        }
        _ => None,
    }
}

pub(super) fn anthropic_usage(usage: &Value) -> Value {
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

pub(super) fn incomplete_reason(value: Option<&Value>) -> Result<Option<&'static str>, AnthropicError> {
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

pub(super) fn tool_arguments(value: Option<&Value>) -> Result<String, AnthropicError> {
    let default = Value::Object(Map::new());
    let Some(value) = object(value.unwrap_or(&default)) else {
        return Err(upstream_error(
            "Anthropic upstream returned invalid tool input",
        ));
    };
    serde_json::to_string(&Value::Object(value.clone()))
        .map_err(|_| upstream_error("Anthropic upstream returned invalid tool input"))
}

pub(super) fn custom_tool_id(call_id: &str) -> String {
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

pub(super) fn custom_tool_input(arguments: &str) -> String {
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
pub(super) fn output_message(id: &str, text: &str) -> Value {
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
