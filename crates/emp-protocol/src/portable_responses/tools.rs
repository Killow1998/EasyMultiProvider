//! Portable Responses tool schemas and projection errors.
use super::{PortableProjectionError, error, python_string};
use crate::tool_definitions::{RawTool, collect_tools, custom_tool_parameters};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub(super) fn raw_tools(
    body: &Map<String, Value>,
    include_loaded: bool,
) -> Result<Vec<RawTool<'_>>, PortableProjectionError> {
    crate::tool_definitions::raw_tools(body, include_loaded)
        .map_err(|_| error(0, "namespace", "invalid_tool_namespace"))
}

pub(super) fn loaded_tools(source: Option<&Value>) -> Result<Vec<Value>, PortableProjectionError> {
    if !source.is_some_and(Value::is_array) {
        return Err(error(0, "additional_tools", "invalid_tools"));
    }
    let mut result = Vec::new();
    collect_tools(source, &[], &mut result)
        .map_err(|_| error(0, "namespace", "invalid_tool_namespace"))?;
    portable_tools(result)
}

pub(super) fn portable_tools(
    source: Vec<RawTool<'_>>,
) -> Result<Vec<Value>, PortableProjectionError> {
    let mut result = Vec::new();
    let mut seen = BTreeMap::new();
    for (index, item) in source.into_iter().enumerate() {
        let kind = python_string(item.value.get("type"), "unknown");
        if !matches!(kind.as_str(), "function" | "custom") {
            return Err(error(index, &kind, "unsupported_tool_definition"));
        }
        let function = item
            .value
            .get("function")
            .and_then(Value::as_object)
            .unwrap_or(item.value);
        let name = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| error(index, &kind, "invalid_tool_definition"))?;
        let parameters = if kind == "custom" {
            custom_tool_parameters()
        } else {
            function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({}))
        };
        if !parameters.is_object() {
            return Err(error(index, &kind, "invalid_tool_definition"));
        }
        let identity = (item.namespace, kind.clone(), function);
        if let Some(previous) = seen.get(name) {
            if previous != &identity {
                return Err(error(index, &kind, "tool_name_collision"));
            }
            continue;
        }
        seen.insert(name, identity);
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
