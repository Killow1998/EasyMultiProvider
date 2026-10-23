//! Request-owned identities for external protocols without Codex tool namespaces.
use crate::collaboration::python_json;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
struct Identity {
    namespace: Option<String>,
    name: String,
    search: bool,
}

#[derive(Debug, Default)]
pub struct ExternalTools {
    preserve_loads: bool,
    identities: BTreeMap<String, Identity>,
    search_name: Option<String>,
    search_indices: BTreeSet<String>,
    search_ids: BTreeSet<String>,
}

impl ExternalTools {
    pub fn with_in_place_loading(preserve_loads: bool) -> Self {
        Self {
            preserve_loads,
            ..Self::default()
        }
    }
    fn has_request_state(&self) -> bool {
        !self.identities.is_empty()
            || self.search_name.is_some()
            || !self.search_indices.is_empty()
            || !self.search_ids.is_empty()
    }

    fn input_requires_preparation(input: &Value) -> bool {
        const TOOL_BRIDGE_ITEMS: &[&str] = &[
            "additional_tools",
            "tool_search_call",
            "tool_search_output",
            "function_call",
            "custom_tool_call",
            "function_call_output",
            "custom_tool_call_output",
        ];
        matches!(
            input.get("type").and_then(Value::as_str),
            Some(kind) if TOOL_BRIDGE_ITEMS.contains(&kind)
        )
    }

    fn body_requires_preparation(body: &Value) -> bool {
        match body.get("tools") {
            None => {}
            Some(Value::Array(tools)) if tools.is_empty() => {}
            Some(_) => return true,
        }
        if body.get("tool_choice").is_some() {
            return true;
        }
        match body.get("input") {
            Some(Value::Object(_)) => true,
            Some(Value::Array(items)) => items.iter().any(Self::input_requires_preparation),
            _ => false,
        }
    }

    /// Borrow ordinary request bodies that require no tool namespace bridge.
    /// Tool-bearing requests retain the existing owned projection and identity
    /// bookkeeping used by response restoration.
    pub fn prepare_or_borrow<'a>(
        &mut self,
        body: &'a Value,
    ) -> Result<Cow<'a, Value>, &'static str> {
        if !self.has_request_state() && !Self::body_requires_preparation(body) {
            return Ok(Cow::Borrowed(body));
        }
        self.prepare(body).map(Cow::Owned)
    }

    fn name(
        &mut self,
        name: &Value,
        namespace: Option<&Value>,
        search: bool,
    ) -> Result<String, &'static str> {
        let name = name
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("request projection failed: invalid tool name")?;
        let namespace = match namespace {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => (!s.is_empty()).then_some(s.as_str()),
            _ => return Err("request projection failed: invalid tool namespace"),
        };
        let identity = Identity {
            namespace: namespace.map(str::to_owned),
            name: name.to_owned(),
            search,
        };
        let alias = if namespace.is_some() || search {
            let digest = format!(
                "{:x}",
                Sha256::digest(python_json(&json!([namespace, name, search])).as_bytes())
            );
            let label = format!(
                "{}{name}",
                namespace.map(|s| format!("{s}_")).unwrap_or_default()
            );
            let label = label
                .chars()
                .take(22)
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>();
            format!("emp_tool_{label}_{}", &digest[..32])
        } else {
            name.to_owned()
        };
        if self
            .identities
            .get(&alias)
            .is_some_and(|previous| previous != &identity)
        {
            return Err("request projection failed: tool name collision");
        }
        self.identities.insert(alias.clone(), identity);
        if search {
            self.search_name = Some(alias.clone());
        }
        Ok(alias)
    }

    pub fn prepare(&mut self, body: &Value) -> Result<Value, &'static str> {
        let mut result = body.clone();
        let definitions = self.definitions(body.get("tools").unwrap_or(&json!([])), None, "")?;
        let items = result
            .get_mut("input")
            .filter(|input| input.is_array() || input.is_object())
            .map(|input| match input.take() {
                Value::Array(items) => items,
                item => vec![item],
            });
        if let Some(items) = items {
            let mut converted = Vec::with_capacity(items.len());
            for mut item in items {
                match item["type"].as_str() {
                    Some("additional_tools" | "tool_search_output") => {
                        let loaded =
                            self.definitions(item.get("tools").unwrap_or(&json!([])), None, "")?;
                        if item["type"] == "additional_tools" {
                            converted.push(json!({"type":"additional_tools", "role":"developer", "tools":loaded}));
                            continue;
                        }
                        if item["execution"] != "client"
                            || item["call_id"].as_str().is_none_or(str::is_empty)
                        {
                            return Err("request projection failed: invalid tool search output");
                        }
                        item = json!({"type":"function_call_output", "call_id":item["call_id"],
                            "output":format!("{{\"status\": {}, \"tools\": {}}}", python_json(&item["status"]), python_json(&json!(loaded)))});
                        converted.push(item);
                        converted.push(
                            json!({"type":"additional_tools", "role":"developer", "tools":loaded}),
                        );
                        continue;
                    }
                    Some("tool_search_call") => {
                        if item["execution"] != "client"
                            || item["call_id"].as_str().is_none_or(str::is_empty)
                            || !item["arguments"].is_object()
                        {
                            return Err("request projection failed: invalid tool search call");
                        }
                        item = json!({"type":"function_call", "call_id":item["call_id"],
                            "name":self.name(&json!("tool_search"),None,true)?, "arguments":python_json(&item["arguments"])});
                    }
                    Some("function_call" | "custom_tool_call" | "function_call_output")
                        if item.get("name").is_some_and(|v| v != "" && !v.is_null()) =>
                    {
                        let namespace = item.as_object_mut().unwrap().remove("namespace");
                        item["name"] =
                            json!(self.name(&item["name"], namespace.as_ref(), false)?);
                    }
                    _ => {}
                }
                converted.push(item);
            }
            result["input"] = Value::Array(converted);
        }
        if body.get("tools").is_some() || !definitions.is_empty() {
            result["tools"] = Value::Array(definitions);
        }
        self.resolve_catalog(&mut result);
        if let Some(mut choice) = result.get("tool_choice").filter(|v| v.is_object()).cloned() {
            if choice["type"] == "allowed_tools" {
                if let Some(choices) = choice["tools"].as_array_mut() {
                    for selected in choices {
                        self.prepare_choice(selected)?;
                    }
                }
                let allowed = choice["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v["name"].as_str())
                    .collect::<BTreeSet<_>>();
                let allowed_tool = |tool: &Value| {
                    tool.get("function").unwrap_or(tool)["name"]
                        .as_str()
                        .is_some_and(|name| allowed.contains(name))
                };
                if let Some(tools) = result["tools"].as_array_mut() {
                    tools.retain(&allowed_tool);
                }
                if self.preserve_loads
                    && let Some(input) = result["input"].as_array_mut()
                {
                    for item in input
                        .iter_mut()
                        .filter(|item| item["type"] == "additional_tools")
                    {
                        if let Some(tools) = item["tools"].as_array_mut() {
                            tools.retain(&allowed_tool);
                        }
                    }
                }
                result["tool_choice"] = choice.get("mode").cloned().unwrap_or(json!("auto"));
            } else {
                self.prepare_choice(&mut choice)?;
                result["tool_choice"] = choice;
            }
        }
        Ok(result)
    }
}

mod catalog;
pub use catalog::removed_tool_names;
mod definitions;
mod restore;
#[cfg(test)]
mod tests;
