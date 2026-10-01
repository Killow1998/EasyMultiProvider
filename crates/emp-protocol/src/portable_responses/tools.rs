//! Portable Responses tool schemas and namespace collection.
use super::{PortableProjectionError, error, python_string};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone)]
pub(super) struct RawTool {
    pub(super) value: Map<String, Value>,
    namespace: Vec<String>,
}

type ToolIdentity = (Vec<String>, String, Map<String, Value>);

fn collect_tools(
    result: &mut Vec<RawTool>,
    source: Option<&Value>,
    namespace: &[String],
) -> Result<(), PortableProjectionError> {
    let Some(source) = source.and_then(Value::as_array) else {
        return Ok(());
    };
    for item in source {
        let Some(item) = item.as_object() else {
            continue;
        };
        if item.get("type").and_then(Value::as_str) == Some("namespace") {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| error(0, "namespace", "invalid_tool_namespace"))?;
            let mut nested = namespace.to_vec();
            nested.push(name.to_owned());
            collect_tools(result, item.get("tools"), &nested)?;
        } else {
            result.push(RawTool {
                value: item.clone(),
                namespace: namespace.to_vec(),
            });
        }
    }
    Ok(())
}

pub(super) fn raw_tools(
    body: &Map<String, Value>,
) -> Result<Vec<RawTool>, PortableProjectionError> {
    let mut result = Vec::new();
    collect_tools(&mut result, body.get("tools"), &[])?;
    let owned;
    let source: &[Value] = match body.get("input") {
        Some(Value::Object(item)) => {
            owned = vec![Value::Object(item.clone())];
            &owned
        }
        Some(Value::Array(items)) => items,
        _ => &[],
    };
    for item in source {
        if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
            collect_tools(&mut result, item.get("tools"), &[])?;
        }
    }
    Ok(result)
}

pub(super) fn portable_tools(source: Vec<RawTool>) -> Result<Vec<Value>, PortableProjectionError> {
    let mut result = Vec::new();
    let mut seen: BTreeMap<String, ToolIdentity> = BTreeMap::new();
    for (index, item) in source.into_iter().enumerate() {
        let kind = python_string(item.value.get("type"), "unknown");
        if !matches!(kind.as_str(), "function" | "custom") {
            return Err(error(index, &kind, "unsupported_tool_definition"));
        }
        let function = item
            .value
            .get("function")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| item.value.clone());
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| error(index, &kind, "invalid_tool_definition"))?;
        let parameters = if kind == "custom" {
            json!({
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
                "additionalProperties": false,
            })
        } else {
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({}))
        };
        if !parameters.is_object() {
            return Err(error(index, &kind, "invalid_tool_definition"));
        }
        let identity = (item.namespace, kind.clone(), function.clone());
        if let Some(previous) = seen.get(name) {
            if previous != &identity {
                return Err(error(index, &kind, "tool_name_collision"));
            }
            continue;
        }
        seen.insert(name.to_owned(), identity);
        let mut tool = Map::from_iter([
            ("type".to_owned(), Value::String("function".to_owned())),
            ("name".to_owned(), Value::String(name.to_owned())),
            (
                "description".to_owned(),
                Value::String(python_string(function.get("description"), "")),
            ),
            ("parameters".to_owned(), parameters),
        ]);
        if function.get("strict").is_some_and(Value::is_boolean) {
            tool.insert("strict".to_owned(), function["strict"].clone());
        }
        result.push(Value::Object(tool));
    }
    Ok(result)
}
