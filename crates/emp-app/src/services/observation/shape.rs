//! Content-free request facts. Names, arguments, text and attachments are omitted.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn kind(value: &Value, default: &str) -> String {
    let value = emp_state::diagnostics::schema::id(value);
    if value.is_empty() {
        default.into()
    } else {
        value
    }
}
pub(super) fn facts(body: &Value, headers: &BTreeMap<String, String>) -> Value {
    let input = match &body["input"] {
        Value::Array(items) => items.iter().take(256).collect::<Vec<_>>(),
        item @ Value::Object(_) => vec![item],
        _ => Vec::new(),
    };
    let mut types = Vec::new();
    let mut parts = Vec::new();
    let mut calls = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    let mut invalid = false;
    let mut standalone = false;
    for item in &input {
        if !item.is_object() {
            types.push("unknown".to_owned());
            continue;
        }
        let item_type = kind(&item["type"], "message");
        types.push(item_type.clone());
        for part in item["content"].as_array().into_iter().flatten() {
            if parts.len() >= 256 {
                break;
            }
            parts.push(if part.is_object() {
                kind(&part["type"], "unknown")
            } else if part.is_string() {
                "text".into()
            } else {
                "unknown".into()
            });
        }
        if matches!(item_type.as_str(), "function_call" | "custom_tool_call") {
            if let Some(id) = item["call_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| item["id"].as_str().filter(|s| !s.is_empty()))
            {
                calls.insert(id);
            } else {
                invalid = true;
            }
        } else if matches!(
            item_type.as_str(),
            "function_call_output" | "custom_tool_call_output"
        ) {
            if item_type == "function_call_output"
                && item["call_id"].is_null()
                && item["name"].as_str().is_some_and(|s| !s.is_empty())
            {
                standalone = true;
                continue;
            }
            if let Some(id) = item["call_id"].as_str().filter(|s| !s.is_empty()) {
                if !outputs.insert(id) || !calls.contains(id) {
                    invalid = true;
                }
            } else {
                invalid = true;
            }
        }
    }
    let pairing = if invalid {
        "invalid"
    } else if calls.is_empty() && outputs.is_empty() {
        if standalone { "standalone" } else { "none" }
    } else if calls == outputs {
        "paired"
    } else {
        "incomplete"
    };
    let mut result = json!({"request_item_count":input.len(),"request_item_types":types,"content_part_types":parts,"tool_pairing_status":pairing});
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.as_str())
    };
    result["request_id"] = json!(header("x-emp-request-id"));
    result["client_kind"] = json!(header("originator"));
    result["session_id"] = json!(header("session-id"));
    let metadata = body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| header("x-codex-turn-metadata"))
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    result["thread_id"] = json!(
        header("thread-id")
            .map(str::to_owned)
            .or_else(|| text(&metadata["thread_id"]))
            .or_else(|| text(&metadata["threadId"]))
            .or_else(|| text(&body["metadata"]["thread_id"]))
            .or_else(|| text(&body["metadata"]["threadId"]))
    );
    result["turn_id"] = json!(text(&metadata["turn_id"]).or_else(|| text(&metadata["turnId"])));
    result["parent_thread_id"] = metadata["forked_from_thread_id"].clone();
    result
}
pub(super) fn request_bytes(body: &Value) -> usize {
    let compact = body.to_string();
    let mut quoted = false;
    let mut escaped = false;
    let mut separators = 0;
    for byte in compact.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b',' || byte == b':' {
            separators += 1;
        }
    }
    compact.len() + separators
}
