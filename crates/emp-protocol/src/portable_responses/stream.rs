//! Incremental Responses SSE projection and custom tool state.

use super::*;
use super::response::{custom_tool_ids, custom_tool_input, project_reasoning_item};

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Bool(true) => true,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_or_string(value: Option<&Value>) -> String {
    value
        .filter(|value| python_truthy(value))
        .map(|value| python_string(Some(value), ""))
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CustomStreamState {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// Request-local projection state for one external Responses SSE stream.
#[derive(Debug, Clone)]
pub struct PortableStreamProjector {
    suppressed_item_ids: BTreeSet<String>,
    custom_names: BTreeSet<String>,
    custom_state: BTreeMap<String, CustomStreamState>,
    preserve_reasoning_summary: bool,
    preserve_reasoning_state: bool,
}

impl PortableStreamProjector {
    pub fn new(
        custom_names: BTreeSet<String>,
        preserve_reasoning_summary: bool,
        preserve_reasoning_state: bool,
    ) -> Self {
        Self {
            suppressed_item_ids: BTreeSet::new(),
            custom_names,
            custom_state: BTreeMap::new(),
            preserve_reasoning_summary,
            preserve_reasoning_state,
        }
    }

    pub fn project(&mut self, event: &Value) -> Result<Option<Value>, PortableProjectionError> {
        let mut projected = object(event)
            .ok_or_else(|| error(0, "event", "invalid_response_output"))?
            .clone();
        let mut event_type = python_or_string(projected.get("type")).to_ascii_lowercase();
        let item = projected.get("item").and_then(Value::as_object).cloned();
        if item
            .as_ref()
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("compaction")
        {
            let index = projected
                .get("output_index")
                .and_then(Value::as_i64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0);
            return Err(error(index, "compaction", "external_compaction"));
        }
        if let Some(item) = item
            .as_ref()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
        {
            let item_id = item.get("id").and_then(Value::as_str);
            let clean = project_reasoning_item(
                item,
                self.preserve_reasoning_summary,
                self.preserve_reasoning_state,
            );
            let Some(clean) = clean else {
                if let Some(item_id) = item_id.filter(|value| !value.is_empty()) {
                    self.suppressed_item_ids.insert(item_id.to_owned());
                }
                return Ok(None);
            };
            projected.insert("item".to_owned(), clean);
        }
        if let Some(item) = item.as_ref().filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call")
                && item
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| self.custom_names.contains(name))
        }) {
            let raw_item_id = item
                .get("id")
                .filter(|value| python_truthy(value))
                .or_else(|| item.get("call_id").filter(|value| python_truthy(value)))
                .map(|value| python_string(Some(value), ""))
                .unwrap_or_default();
            let raw_id = Value::String(raw_item_id.clone());
            let (item_id, call_id) = custom_tool_ids(Some(&raw_id), item.get("call_id"));
            let state = self
                .custom_state
                .entry(raw_item_id)
                .or_insert_with(|| CustomStreamState {
                    item_id,
                    call_id,
                    name: python_or_string(item.get("name")),
                    arguments: String::new(),
                });
            let mut clean = item.clone();
            clean.insert("id".to_owned(), Value::String(state.item_id.clone()));
            clean.insert("call_id".to_owned(), Value::String(state.call_id.clone()));
            clean.insert(
                "type".to_owned(),
                Value::String("custom_tool_call".to_owned()),
            );
            let arguments = clean
                .remove("arguments")
                .unwrap_or(Value::String(String::new()));
            if python_truthy(&arguments) {
                state.arguments = python_string(Some(&arguments), "");
            }
            clean.insert(
                "input".to_owned(),
                Value::String(custom_tool_input(Some(&Value::String(
                    state.arguments.clone(),
                )))),
            );
            projected.insert("item".to_owned(), Value::Object(clean));
        }
        let raw_item_id = python_or_string(projected.get("item_id"));
        if let Some(state) = self.custom_state.get_mut(&raw_item_id) {
            event_type = python_or_string(projected.get("type"));
            if event_type == "response.function_call_arguments.delta" {
                state
                    .arguments
                    .push_str(&python_or_string(projected.get("delta")));
                return Ok(None);
            }
            if event_type == "response.function_call_arguments.done" {
                if let Some(arguments) = projected
                    .get("arguments")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    state.arguments = arguments.to_owned();
                }
                projected.insert(
                    "type".to_owned(),
                    Value::String("response.custom_tool_call_input.delta".to_owned()),
                );
                projected.insert("item_id".to_owned(), Value::String(state.item_id.clone()));
                projected.remove("arguments");
                projected.insert(
                    "delta".to_owned(),
                    Value::String(custom_tool_input(Some(&Value::String(
                        state.arguments.clone(),
                    )))),
                );
            } else {
                projected.insert("item_id".to_owned(), Value::String(state.item_id.clone()));
            }
        }
        if projected
            .get("item_id")
            .and_then(Value::as_str)
            .is_some_and(|item_id| self.suppressed_item_ids.contains(item_id))
        {
            return Ok(None);
        }
        let summary_event = event_type.starts_with("response.reasoning_summary_");
        if summary_event && !self.preserve_reasoning_summary {
            return Ok(None);
        }
        if !summary_event && (event_type.contains("reasoning") || event_type.contains("thinking")) {
            return Ok(None);
        }
        if projected
            .get("part")
            .and_then(Value::as_object)
            .and_then(|part| part.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| {
                matches!(
                    kind.to_ascii_lowercase().as_str(),
                    "reasoning" | "reasoning_text" | "thinking" | "thinking_text"
                )
            })
        {
            return Ok(None);
        }
        if let Some(response) = projected.get("response").cloned()
            && response.is_object()
        {
            projected.insert(
                "response".to_owned(),
                project_response(
                    &response,
                    &self.custom_names,
                    self.preserve_reasoning_summary,
                    self.preserve_reasoning_state,
                )?,
            );
        }
        Ok(Some(Value::Object(projected)))
    }
}
