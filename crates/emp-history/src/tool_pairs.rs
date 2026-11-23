use crate::{HistoryError, VisibleItem};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Endpoint<'a> {
    Call(&'static str, &'a str),
    Output(&'static str, &'a str),
}

fn hosted_search(kind: &str, item: &Value) -> bool {
    matches!(kind, "tool_search_call" | "tool_search_output") && item["execution"] == "server"
}

impl<'a> Endpoint<'a> {
    fn from_type(kind: &str, id: &'a str) -> Option<Self> {
        Some(match kind {
            "function_call" | "tool_call" => Self::Call("function_call_output", id),
            "function_call_output" | "tool_result" | "tool_output" | "mcp_tool_result" => {
                Self::Output("function_call_output", id)
            }
            "custom_tool_call" => Self::Call("custom_tool_call_output", id),
            "custom_tool_call_output" => Self::Output("custom_tool_call_output", id),
            "tool_search_call" => Self::Call("tool_search_output", id),
            "tool_search_output" => Self::Output("tool_search_output", id),
            _ => return None,
        })
    }

    pub(crate) fn from_wire(item: &'a Value) -> Option<Self> {
        let kind = item["type"].as_str()?;
        if hosted_search(kind, item) {
            return None;
        }
        let id = item
            .get("call_id")
            .or_else(|| item.get("callId"))?
            .as_str()?;
        Self::from_type(kind, id)
    }

    fn from_history(item: &'a VisibleItem) -> Result<Option<Self>, HistoryError> {
        let default = match item.kind.as_str() {
            "tool_call" => "function_call",
            "tool_result" => "function_call_output",
            _ => return Ok(None),
        };
        let kind = item.raw_type.as_deref().unwrap_or(default);
        if hosted_search(kind, &item.content) {
            return Ok(None);
        }
        let id = item
            .call_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| HistoryError::new("tool_call_identity_missing"))?;
        Ok(Self::from_type(kind, id))
    }

    fn counterpart(self) -> Self {
        match self {
            Self::Call(kind, id) => Self::Output(kind, id),
            Self::Output(kind, id) => Self::Call(kind, id),
        }
    }
}

pub(super) fn normalize_tool_pairs(
    history: &[VisibleItem],
    request: &[Value],
) -> Result<Vec<VisibleItem>, HistoryError> {
    let retained: BTreeSet<_> = request
        .iter()
        .filter_map(Value::as_object)
        .filter_map(crate::wire_replay_key)
        .collect();
    let mut endpoints: BTreeSet<_> = request.iter().filter_map(Endpoint::from_wire).collect();
    let mut items = Vec::with_capacity(history.len());
    for item in history {
        if item.replay_key().is_some_and(|key| retained.contains(&key)) {
            continue;
        }
        let endpoint = Endpoint::from_history(item)?;
        endpoints.extend(endpoint);
        items.push((item, endpoint));
    }

    let mut restored = Vec::with_capacity(items.len());
    for (item, endpoint) in items {
        if let Some(endpoint) = endpoint
            && !endpoints.contains(&endpoint.counterpart())
        {
            if let Endpoint::Call(output_type, _) = endpoint {
                restored.push(item.clone());
                restored.push(VisibleItem {
                    kind: "tool_result".to_owned(),
                    content: if output_type == "tool_search_output" {
                        json!({"status":"completed","execution":"client","tools":[]})
                    } else {
                        json!({"output":"aborted"})
                    },
                    item_id: None,
                    turn_id: item.turn_id.clone(),
                    call_id: item.call_id.clone(),
                    raw_type: Some(output_type.to_owned()),
                });
            }
            continue;
        }
        restored.push(item.clone());
    }
    Ok(restored)
}
