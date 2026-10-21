//! Projects supported tool definitions and explicit tool choices.
use super::ExternalTools;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Validate one schema per alias across both catalog locations, preserving the
/// first top-level occurrence and every in-place loading boundary.
pub(super) fn unique_definitions(
    definitions: Vec<Value>,
    input: &Value,
    preserve_loads: bool,
) -> Result<Vec<Value>, &'static str> {
    let loaded = input
        .as_array()
        .filter(|_| preserve_loads)
        .into_iter()
        .flatten()
        .filter(|item| item["type"] == "additional_tools")
        .flat_map(|item| item["tools"].as_array().into_iter().flatten());
    let mut seen = BTreeMap::new();
    let mut keep = vec![true; definitions.len()];
    for (index, tool) in definitions.iter().chain(loaded).enumerate() {
        let definition = tool.get("function").unwrap_or(tool);
        let key = definition
            .get("name")
            .filter(|value| !value.is_null())
            .map(Value::to_string)
            .unwrap_or_else(|| super::python_json(tool));
        if let Some(previous) = seen.insert(key, tool) {
            if previous != tool {
                return Err("request projection failed: conflicting tool definitions");
            }
            if let Some(keep) = keep.get_mut(index) {
                *keep = false;
            }
        }
    }
    Ok(definitions
        .into_iter()
        .zip(keep)
        .filter_map(|(tool, keep)| keep.then_some(tool))
        .collect())
}

impl ExternalTools {
    pub(super) fn definitions(
        &mut self,
        tools: &Value,
        namespace: Option<&str>,
        description: &str,
    ) -> Result<Vec<Value>, &'static str> {
        let tools = tools
            .as_array()
            .ok_or("request projection failed: invalid tools")?;
        let mut result = Vec::new();
        for raw in tools {
            let mut tool = raw
                .as_object()
                .cloned()
                .ok_or("request projection failed: invalid tool definition")?;
            match raw["type"].as_str() {
                Some("namespace") => {
                    let name = raw["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .filter(|_| namespace.is_none())
                        .ok_or("request projection failed: invalid tool namespace")?;
                    result.extend(self.definitions(
                        &raw["tools"],
                        Some(name),
                        raw["description"].as_str().unwrap_or(""),
                    )?);
                    continue;
                }
                Some("tool_search") => {
                    if raw["execution"] != "client" {
                        return Err("external tool search requires client execution");
                    }
                    result.push(json!({"type":"function", "name":self.name(&json!("tool_search"),None,true)?,
                        "description":raw.get("description").cloned().unwrap_or(json!("")),
                        "parameters":raw.get("parameters").cloned().unwrap_or(json!({}))}));
                    continue;
                }
                Some("web_search") => {
                    // Codex executes this server-side tool. An external
                    // provider cannot execute it, so omit its schema.
                    continue;
                }
                Some("function" | "custom") => {
                    let definition = if tool.get("function").is_some_and(Value::is_object) {
                        tool.get_mut("function").unwrap().as_object_mut().unwrap()
                    } else {
                        &mut tool
                    };
                    if let Some(namespace) = namespace {
                        let label = format!(
                            "{namespace}.{}",
                            definition.get("name").and_then(Value::as_str).unwrap_or("")
                        );
                        let own = definition
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let joined = [label.as_str(), description, own]
                            .into_iter()
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        definition.insert("description".into(), json!(joined));
                    }
                    let alias = self.name(
                        definition.get("name").unwrap_or(&Value::Null),
                        namespace.map(|s| json!(s)).as_ref(),
                        false,
                    )?;
                    definition.insert("name".into(), json!(alias));
                    definition.remove("defer_loading");
                    tool.remove("defer_loading");
                }
                _ => {}
            }
            result.push(Value::Object(tool));
        }
        Ok(result)
    }

    pub(super) fn prepare_choice(&mut self, item: &mut Value) -> Result<(), &'static str> {
        if item["type"] == "tool_search" {
            item["type"] = json!("function");
            item["name"] = json!(self.name(&json!("tool_search"), None, true)?);
        } else if item["name"].as_str().is_some_and(|s| !s.is_empty()) {
            let namespace = item.as_object_mut().unwrap().remove("namespace");
            item["name"] = json!(self.name(&item["name"], namespace.as_ref(), false)?);
        }
        Ok(())
    }
}
