//! Project visible items into Codex-compatible request items.
use crate::{CHECKPOINT_PREFIX, VisibleItem};
use serde_json::{Map, Value, json};

pub(super) fn wire_item(item: &VisibleItem) -> Option<Value> {
    let text = || content_text(&item.content).unwrap_or_default();
    match item.kind.as_str() {
        "user_message" => Some(message_with_content("user", &item.content)),
        "assistant_message" => Some(message_with_content("assistant", &item.content)),
        "compaction_summary" | "compaction_marker" => {
            let text = text();
            (!text.trim().is_empty())
                .then(|| message("user", &format!("{CHECKPOINT_PREFIX}\n\n{}", text.trim())))
        }
        "tool_call" | "tool_result" | "standalone_tool_output" => {
            let standalone = item.kind == "standalone_tool_output";
            let mut value =
                item.content.as_object().cloned().unwrap_or_else(|| {
                    Map::from_iter([("output".to_owned(), item.content.clone())])
                });
            let raw_type = if standalone {
                "function_call_output".to_owned()
            } else {
                item.raw_type.clone().unwrap_or_else(|| {
                    if item.kind == "tool_call" {
                        "function_call".to_owned()
                    } else {
                        "function_call_output".to_owned()
                    }
                })
            };
            value.insert("type".to_owned(), Value::String(raw_type));
            if standalone {
                value.remove("call_id");
            } else if let Some(call_id) = &item.call_id {
                value.insert("call_id".to_owned(), Value::String(call_id.clone()));
            }
            if !standalone && let Some(item_id) = &item.item_id {
                value.insert("id".to_owned(), Value::String(item_id.clone()));
            }
            Some(Value::Object(value))
        }
        _ => {
            let text = text();
            (!text.trim().is_empty()).then(|| {
                message(
                    "user",
                    &format!("Visible {}: {}", item.kind.replace('_', " "), text.trim()),
                )
            })
        }
    }
}

fn message_with_content(role: &str, content: &Value) -> Value {
    if let Value::Array(parts) = content {
        return json!({"type":"message","role":role,"content":parts});
    }
    if let Value::Object(value) = content
        && matches!(
            value.get("content"),
            Some(Value::String(_) | Value::Array(_))
        )
    {
        return message_with_content(role, &value["content"]);
    }
    message(role, &content_text(content).unwrap_or_default())
}

pub(super) fn message(role: &str, text: &str) -> Value {
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type":"message","role":role,"content":[{"type":part_type,"text":text}]})
}

pub(super) fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => Some(
            values
                .iter()
                .filter_map(content_text)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Object(value) => {
            for key in ["text", "message", "content", "output", "result"] {
                if let Some(text) = value.get(key).and_then(content_text)
                    && !text.is_empty()
                {
                    return Some(text);
                }
            }
            serde_json::to_string(value).ok()
        }
        value => Some(value.to_string()),
    }
}
