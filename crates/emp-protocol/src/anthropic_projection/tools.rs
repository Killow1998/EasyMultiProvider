//! Anthropic tool schema, arguments, namespace and tool-choice projection.
use super::{AnthropicError, request_error, required_string};
use serde_json::{Map, Value, json};
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

pub(super) fn custom_tool_arguments(value: Option<&Value>) -> Result<Value, AnthropicError> {
    let input = match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => serde_json::to_string(value)
            .map_err(|_| request_error("request projection failed: invalid custom tool input"))?,
        None => String::new(),
    };
    serde_json::to_value(serde_json::json!({"input": input}))
        .map_err(|_| request_error("request projection failed: invalid custom tool input"))
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
            if let Some(item) = item.as_object() {
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

pub(super) fn tools(body: &Map<String, Value>) -> Result<Vec<Value>, AnthropicError> {
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
        let function = item
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(item);
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
