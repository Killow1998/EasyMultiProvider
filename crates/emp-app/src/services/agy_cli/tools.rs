//! Four client capabilities. Never invent a tool the downstream did not offer.
use super::Error;
use serde_json::{Value, json};

const CAPABILITIES: &[&[&str]] = &[
    &["read_file", "read", "view_file"],
    &["write_file", "write", "write_to_file"],
    &["apply_patch", "edit_file", "edit", "replace_file_content"],
    &["exec_command", "shell", "bash", "run_command"],
];

#[derive(Clone)]
pub(super) struct Tool {
    pub(super) name: String,
    pub(super) namespace: String,
    pub(super) custom: bool,
    pub(super) definition: Value,
}

pub(super) fn select(body: &Value) -> Result<Vec<Tool>, Error> {
    let choice = &body["tool_choice"];
    if !(choice.is_null()
        || matches!(choice.as_str(), Some("none" | "auto" | "required"))
        || matches!(choice["type"].as_str(), Some("function" | "custom"))
        || choice["type"] == "allowed_tools"
            && choice["tools"].is_array()
            && (choice["mode"].is_null()
                || matches!(choice["mode"].as_str(), Some("auto" | "required"))))
    {
        return Err(Error("agy_cli_invalid_input"));
    }
    let mut offered = Vec::new();
    collect(body.get("tools"), "", &mut offered)?;
    for item in body["input"].as_array().into_iter().flatten() {
        if matches!(
            item["type"].as_str(),
            Some("additional_tools" | "tool_search_output")
        ) {
            collect(item.get("tools"), "", &mut offered)?;
        }
    }
    let mut selected = Vec::new();
    for aliases in CAPABILITIES {
        if let Some(tool) = aliases.iter().find_map(|name| {
            offered
                .iter()
                .find(|t| t.name == *name && allowed(&body["tool_choice"], t))
        }) {
            selected.push(tool.clone());
        }
    }
    if requires_call(choice) && selected.is_empty() {
        return Err(Error("agy_cli_tool_choice_unavailable"));
    }
    Ok(selected)
}

pub(super) fn requires_call(choice: &Value) -> bool {
    choice == "required"
        || matches!(choice["type"].as_str(), Some("function" | "custom"))
        || choice["type"] == "allowed_tools" && choice["mode"] == "required"
}

fn collect(value: Option<&Value>, namespace: &str, output: &mut Vec<Tool>) -> Result<(), Error> {
    let Some(value) = value else {
        return Ok(());
    };
    let values = value.as_array().ok_or(Error("agy_cli_invalid_input"))?;
    for value in values {
        if value["type"] == "namespace" {
            if !namespace.is_empty() {
                return Err(Error("agy_cli_invalid_input"));
            }
            let name = value["name"]
                .as_str()
                .ok_or(Error("agy_cli_invalid_input"))?;
            collect(value.get("tools"), name, output)?;
        } else if matches!(value["type"].as_str(), Some("function" | "custom")) {
            let definition = value
                .get("function")
                .filter(|v| v.is_object())
                .unwrap_or(value);
            let name = definition["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(Error("agy_cli_invalid_input"))?;
            output.push(Tool {name:name.into(),namespace:namespace.into(),custom:value["type"]=="custom",definition:json!({"type":value["type"],"namespace":namespace,"name":name,"definition":value})});
        }
    }
    Ok(())
}

fn allowed(choice: &Value, tool: &Tool) -> bool {
    if choice.is_null() || matches!(choice.as_str(), Some("auto" | "required")) {
        return true;
    }
    if choice == "none" {
        return false;
    }
    if choice["type"] == "allowed_tools" {
        return choice["tools"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| allowed(item, tool)));
    }
    matches!(choice["type"].as_str(), Some("function" | "custom"))
        && choice["name"] == tool.name
        && choice["namespace"].as_str().unwrap_or("") == tool.namespace
        && (choice["type"] == "custom") == tool.custom
}

pub(super) fn schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
        "answer":{"type":"string"},"tool_calls":{"type":"array","maxItems":32,"items":{
            "type":"object","additionalProperties":false,"properties":{
                "name":{"type":"string"},"namespace":{"type":"string"},
                "arguments":{"type":"object"},"input":{"type":"string"}
            },"required":["name","namespace","arguments","input"]
        }}},"required":["answer","tool_calls"]})
}
