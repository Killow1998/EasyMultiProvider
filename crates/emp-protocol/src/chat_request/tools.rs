//! Chat Completions tool schemas and tool-choice projection.

use super::{ProtocolError, request_error, required_string};
use crate::tool_definitions::{custom_tool_parameters, description, raw_tools};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) fn tools(body: &Map<String, Value>) -> Result<Vec<Value>, ProtocolError> {
    let mut seen = BTreeMap::new();
    let mut result = Vec::new();
    for item in raw_tools(body, true).map_err(request_error)? {
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
            .unwrap_or(item.value);
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
        let identity = (item.namespace, tool_type, function);
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
            "type": "function",
            "function": {
                "name": name,
                "description": description(function.get("description")),
                "parameters": parameters,
            }
        }));
    }
    Ok(result)
}

pub(super) fn tool_choice(value: &Value) -> Result<Value, ProtocolError> {
    if let Some(value @ ("auto" | "none" | "required")) = value.as_str() {
        return Ok(Value::String(value.to_owned()));
    }
    if let Some(value) = value.as_object()
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
