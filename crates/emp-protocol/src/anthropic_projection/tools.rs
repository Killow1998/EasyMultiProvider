//! Anthropic tool schema, arguments, namespace and tool-choice projection.
use super::{AnthropicError, request_error, required_string};
use crate::tool_definitions::{custom_tool_parameters, description, raw_tools};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) fn request_tool_arguments(value: Option<&Value>) -> Result<Value, AnthropicError> {
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

pub(super) fn tools(body: &Map<String, Value>) -> Result<Vec<Value>, AnthropicError> {
    let mut result = Vec::new();
    let mut seen = BTreeMap::new();
    for raw in raw_tools(body, true).map_err(request_error)? {
        let item = raw.value;
        let tool_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(tool_type, "function" | "custom") {
            return Err(request_error(
                "request projection failed: unsupported tool definition",
            ));
        }
        let function = item
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(item);
        let name = required_string(
            function.get("name"),
            "request projection failed: invalid tool definition",
        )?;
        let parameters = if tool_type == "custom" {
            custom_tool_parameters()
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
        let identity = (raw.namespace, tool_type, function);
        if let Some(previous) = seen.get(name) {
            if previous != &identity {
                return Err(request_error(
                    "request projection failed: tool name collision",
                ));
            }
            continue;
        }
        seen.insert(name, identity);
        result.push(serde_json::json!({
            "name": name,
            "description": description(function.get("description")),
            "input_schema": parameters,
        }));
    }
    Ok(result)
}

pub(super) fn tool_choice(
    value: &Value,
    parallel: Option<&Value>,
) -> Result<Value, AnthropicError> {
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
