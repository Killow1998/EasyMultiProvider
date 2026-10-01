//! Responses request controls and Anthropic request assembly.
use super::input::{messages, request_text};
use super::tools::{tool_choice, tools};
use super::{AnthropicError, python_truthy, request_error};
use serde_json::{Map, Value};

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
        .filter(|value| !value.is_empty());
    let schema = format.get("schema").filter(|schema| schema.is_object());
    if matches!(format.get("strict"), Some(value) if !value.is_boolean() && !value.is_null()) {
        return Err(request_error(
            "request projection failed: invalid structured output format",
        ));
    }
    let (Some(_), Some(schema)) = (name, schema) else {
        return Err(request_error(
            "request projection failed: invalid structured output format",
        ));
    };
    Ok(Some(
        serde_json::json!({"type": "json_schema", "schema": schema.clone()}),
    ))
}
pub fn responses_to_anthropic(body: &Value, upstream_model: &str) -> Result<Value, AnthropicError> {
    let Some(body) = body.as_object() else {
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
        .and_then(Value::as_object)
        .and_then(|reasoning| reasoning.get("effort"))
    {
        if effort.is_number() {
            return Err(request_error(
                "request projection failed: numeric reasoning effort is unsupported by Anthropic",
            ));
        }
        if let Some(effort) = effort.as_str() {
            if !matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") {
                return Err(request_error(
                    "request projection failed: unsupported Anthropic reasoning effort",
                ));
            }
            output_config.insert("effort".to_owned(), Value::String(effort.to_owned()));
        }
    }
    if !output_config.is_empty() {
        payload["output_config"] = Value::Object(output_config);
    }
    Ok(payload)
}
