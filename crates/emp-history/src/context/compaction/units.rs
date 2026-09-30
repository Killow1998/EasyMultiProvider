//! Atomic turn and tool-pair boundaries; a tool result stays with its call.
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn split_atomic_units(items: &[Value], tool_exchanges: bool) -> Vec<Vec<Value>> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    let mut current_turn = None::<String>;
    let mut open_calls = BTreeSet::new();
    let mut completed_batch = false;
    for item in items {
        let item_turn = item
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let boundary = !current.is_empty()
            && open_calls.is_empty()
            && ((tool_exchanges && completed_batch)
                || match (&item_turn, &current_turn) {
                    (Some(item_turn), Some(current_turn)) => item_turn != current_turn,
                    _ => is_user_item(item) && current.iter().any(is_user_item),
                });
        if boundary {
            result.push(std::mem::take(&mut current));
            current_turn = None;
            completed_batch = false;
        }
        if current_turn.is_none() {
            current_turn = item_turn;
        }
        current.push(item.clone());
        let call_id = item.get("call_id").and_then(Value::as_str);
        match (tool_role(item), call_id) {
            (Some("call"), Some(call_id)) => {
                open_calls.insert(call_id.to_owned());
            }
            (Some("result"), Some(call_id)) => {
                let matched = open_calls.remove(call_id);
                completed_batch = matched && open_calls.is_empty();
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

pub(super) fn flatten(units: &[Vec<Value>]) -> Vec<Value> {
    units.iter().flatten().cloned().collect()
}

fn is_user_item(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
        || matches!(
            item.get("type").and_then(Value::as_str),
            Some("user_message" | "user_input")
        )
}

pub(super) fn tool_role(item: &Value) -> Option<&'static str> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        _ if kind.ends_with("_call") => Some("call"),
        _ if kind.ends_with("_result")
            || matches!(
                kind,
                "tool_output"
                    | "tool_return"
                    | "function_call_output"
                    | "custom_tool_call_output"
                    | "tool_search_output"
            ) =>
        {
            Some("result")
        }
        _ => None,
    }
}

pub(super) fn visible_candidate(item: &Value) -> bool {
    if item.get("visible") == Some(&Value::Bool(false))
        || item.get("hidden") == Some(&Value::Bool(true))
    {
        return false;
    }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    !matches!(
        kind,
        "analysis"
            | "chain_of_thought"
            | "hidden_cot"
            | "hidden_reasoning"
            | "reasoning"
            | "thinking"
    )
}
