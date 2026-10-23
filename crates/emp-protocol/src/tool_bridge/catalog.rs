//! Resolve chronological declarations without changing saved tool call history.
use super::ExternalTools;
use super::definitions::tool_key;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

impl ExternalTools {
    pub(super) fn resolve_catalog(&self, body: &mut Value) {
        let mut active = BTreeMap::new();
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        let mut update = |tools: &Value, active: &mut BTreeMap<String, Value>| {
            for tool in tools.as_array().into_iter().flatten() {
                let key = tool_key(tool);
                if seen.insert(key.clone()) {
                    order.push(key.clone());
                }
                active.insert(key, tool.clone());
            }
        };
        update(&body["tools"], &mut active);
        for item in body["input"].as_array().into_iter().flatten() {
            if item["type"] == "additional_tools" {
                update(&item["tools"], &mut active);
            } else {
                for removed in removed_tool_names(item) {
                    active.retain(|alias, _| {
                        self.identities.get(alias).is_none_or(|identity| {
                            let qualified = identity.namespace.as_ref().map_or_else(
                                || identity.name.clone(),
                                |namespace| format!("{namespace}.{}", identity.name),
                            );
                            removed != qualified && identity.namespace.as_deref() != Some(removed)
                        })
                    });
                }
            }
        }
        if self.preserve_loads {
            // Keep loading boundaries for destinations that support them, but never
            // offer an obsolete schema or a removed tool alongside the active one.
            let keep = |tool: &Value| active.get(&tool_key(tool)) == Some(tool);
            if let Some(tools) = body["tools"].as_array_mut() {
                tools.retain(&keep);
            }
            if let Some(items) = body["input"].as_array_mut() {
                for item in items.iter_mut().filter(|i| i["type"] == "additional_tools") {
                    if let Some(tools) = item["tools"].as_array_mut() {
                        tools.retain(&keep);
                    }
                }
                items.retain(|item| {
                    item["type"] != "additional_tools"
                        || item["tools"]
                            .as_array()
                            .is_some_and(|tools| !tools.is_empty())
                });
            }
        } else {
            if body.get("tools").is_some() || !order.is_empty() {
                body["tools"] = Value::Array(
                    order
                        .into_iter()
                        .filter_map(|key| active.remove(&key))
                        .collect(),
                );
            }
            if let Some(items) = body["input"].as_array_mut() {
                items.retain(|item| item["type"] != "additional_tools");
            }
        }
    }
}

/// Retired tool or namespace names from a Codex developer catalog notice.
pub fn removed_tool_names(item: &Value) -> impl Iterator<Item = &str> {
    let kinds = &item["internal_chat_message_metadata_passthrough"]["content_item_kinds"];
    item["content"]
        .as_array()
        .filter(|_| item["role"] == "developer")
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, part)| part["type"] == "input_text")
        .filter_map(move |(index, part)| {
            let text = part["text"].as_str()?;
            match kinds.get(index) {
                Some(kind) if kind == "tools.removed_definition" => Some(
                    text.lines()
                        .filter_map(|line| line.strip_prefix("- "))
                        .collect(),
                ),
                None => default_removal_names(text),
                _ => None,
            }
        })
        .flatten()
}

fn default_removal_names(text: &str) -> Option<Vec<&str>> {
    // Codex strips internal metadata for custom providers. Accept its complete
    // default notice grammar there, never a quoted fragment or a user message.
    const HEADERS: &[&str] = &[
        "The following tools are no longer available. Do not call them:",
        "The following namespaces are no longer available. Do not call tools in them unless those tools are declared in a later update:",
    ];
    let mut names = Vec::new();
    for section in text.split("\n\n") {
        let mut lines = section.lines();
        if !HEADERS.contains(&lines.next()?) {
            return None;
        }
        let mut count = 0;
        for line in lines {
            let name = line.strip_prefix("- ").filter(|name| !name.is_empty())?;
            names.push(name);
            count += 1;
        }
        if count == 0 {
            return None;
        }
    }
    Some(names)
}
