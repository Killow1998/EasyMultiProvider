//! Pair visible tool calls and outputs; synthesize aborted outputs where required.
use crate::{HistoryError, VisibleItem};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn normalize_tool_pairs(
    items: Vec<VisibleItem>,
) -> Result<Vec<VisibleItem>, HistoryError> {
    let calls = items
        .iter()
        .filter(|item| tool_role(item) == Some("call"))
        .map(tool_key)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let results = items
        .iter()
        .filter(|item| tool_role(item) == Some("result"))
        .map(tool_key)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut normalized = Vec::new();
    for item in items {
        let role = tool_role(&item);
        let key = role.map(|_| tool_key(&item)).transpose()?;
        if role == Some("result")
            && !calls.contains(key.as_ref().expect("tool role has key"))
            && !is_server_search_output(&item)
        {
            continue;
        }
        normalized.push(item.clone());
        if role == Some("call") && !results.contains(key.as_ref().expect("tool role has key")) {
            normalized.push(aborted_output(&item));
        }
    }
    Ok(normalized)
}

fn tool_role(item: &VisibleItem) -> Option<&'static str> {
    match item.kind.as_str() {
        "tool_call" | "function_call" | "command_call" => Some("call"),
        "tool_result" | "function_result" | "command_result" => Some("result"),
        _ if item.kind.ends_with("_call") => Some("call"),
        _ if item.kind.ends_with("_result") => Some("result"),
        _ => None,
    }
}

fn tool_key(item: &VisibleItem) -> Result<(String, String), HistoryError> {
    let call_id = item
        .call_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| HistoryError::new("tool_call_identity_missing"))?;
    let family = if item
        .raw_type
        .as_deref()
        .unwrap_or_default()
        .contains("custom_tool")
    {
        "custom"
    } else {
        "function"
    };
    Ok((family.to_owned(), call_id.to_owned()))
}

fn is_server_search_output(item: &VisibleItem) -> bool {
    item.raw_type.as_deref() == Some("tool_search_output")
        && item.content.get("execution").and_then(Value::as_str) == Some("server")
}

fn aborted_output(call: &VisibleItem) -> VisibleItem {
    let search = call.raw_type.as_deref() == Some("tool_search_call");
    VisibleItem {
        kind: "tool_result".to_owned(),
        content: if search {
            json!({"status":"completed","execution":"client","tools":[]})
        } else {
            json!({"output":"aborted"})
        },
        item_id: None,
        turn_id: call.turn_id.clone(),
        call_id: call.call_id.clone(),
        raw_type: Some(if search {
            "tool_search_output".to_owned()
        } else if call
            .raw_type
            .as_deref()
            .unwrap_or_default()
            .contains("custom_tool")
        {
            "custom_tool_call_output".to_owned()
        } else {
            "function_call_output".to_owned()
        }),
    }
}
