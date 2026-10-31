//! Shared namespace traversal and custom-input schema for external protocols.

use serde_json::{Map, Value, json};

pub(crate) struct RawTool<'a> {
    pub(crate) value: &'a Map<String, Value>,
    pub(crate) namespace: Vec<String>,
}

pub(crate) fn collect_tools<'a>(
    source: Option<&'a Value>,
    namespace: &[String],
    result: &mut Vec<RawTool<'a>>,
) -> Result<(), &'static str> {
    for item in source.and_then(Value::as_array).into_iter().flatten() {
        let Some(item) = item.as_object() else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) == Some("namespace") {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("request projection failed: invalid tool namespace")?;
            let mut nested = namespace.to_vec();
            nested.push(name.to_owned());
            collect_tools(item.get("tools"), &nested, result)?;
        } else {
            result.push(RawTool {
                value: item,
                namespace: namespace.to_vec(),
            });
        }
    }
    Ok(())
}

pub(crate) fn raw_tools(
    body: &Map<String, Value>,
    include_loaded: bool,
) -> Result<Vec<RawTool<'_>>, &'static str> {
    let mut result = Vec::new();
    collect_tools(body.get("tools"), &[], &mut result)?;
    if include_loaded {
        let input = match body.get("input") {
            Some(Value::Array(items)) => items.as_slice(),
            Some(item @ Value::Object(_)) => std::slice::from_ref(item),
            _ => &[],
        };
        for item in input {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                collect_tools(item.get("tools"), &[], &mut result)?;
            }
        }
    }
    Ok(result)
}

pub(crate) fn custom_tool_parameters() -> Value {
    json!({
        "type": "object", "properties": {"input": {"type": "string"}},
        "required": ["input"], "additionalProperties": false,
    })
}

pub(crate) fn custom_tool_arguments(value: Option<&Value>) -> Value {
    let input = match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    };
    json!({"input": input})
}

pub(crate) fn description(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(Value::Bool(true)) => "True".to_owned(),
        Some(value) => value.to_string(),
    }
}
