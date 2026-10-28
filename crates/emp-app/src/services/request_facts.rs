//! Content-free request facts. Names, arguments, text and attachments are omitted.
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
fn kind(value: &Value, default: &str) -> String {
    let value = emp_state::diagnostics::schema::id(value);
    if value.is_empty() {
        default.into()
    } else {
        value
    }
}
pub(super) fn facts(
    body: &Value,
    headers: &BTreeMap<String, String>,
    memory_kind: Option<&str>,
) -> Map<String, Value> {
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
        // Server search results are owned by the upstream, not a Codex tool pair.
        if matches!(
            item_type.as_str(),
            "tool_search_call" | "tool_search_output"
        ) && item["execution"] == "server"
        {
            continue;
        }
        if matches!(
            item_type.as_str(),
            "function_call" | "custom_tool_call" | "tool_search_call"
        ) {
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
            "function_call_output" | "custom_tool_call_output" | "tool_search_output"
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
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.as_str())
    };
    let metadata = body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| header("x-codex-turn-metadata"))
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let text = |value: &Value| value.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    let thread = header("thread-id")
        .map(str::to_owned)
        .or_else(|| text(&metadata["thread_id"]))
        .or_else(|| text(&metadata["threadId"]))
        .or_else(|| text(&body["metadata"]["thread_id"]))
        .or_else(|| text(&body["metadata"]["threadId"]));
    let turn = text(&metadata["turn_id"])
        .or_else(|| text(&metadata["turnId"]))
        .or_else(|| header("x-codex-image-turn-id").map(str::to_owned));
    let parent = header("x-codex-parent-thread-id")
        .map(str::to_owned)
        .or_else(|| text(&metadata["forked_from_thread_id"]));
    let model = body["model"].as_str().unwrap_or("");
    let session_kind = if memory_kind == Some("memory_consolidation") {
        json!("memory_consolidation")
    } else if model == "codex-auto-review" || model.ends_with("/codex-auto-review") {
        json!("review")
    } else if text(&metadata["forked_from_thread_id"]).is_some() {
        json!("sidechat")
    } else {
        Value::Null
    };
    [
        ("request_item_count", json!(input.len())),
        ("request_item_types", json!(types)),
        ("content_part_types", json!(parts)),
        ("tool_pairing_status", json!(pairing)),
        ("request_id", json!(header("x-emp-request-id"))),
        (
            "has_previous_response",
            json!(
                body.get("previous_response_id")
                    .is_some_and(|id| !id.is_null())
            ),
        ),
        ("client_kind", json!(header("originator"))),
        ("session_id", json!(header("session-id"))),
        ("thread_id", json!(thread)),
        ("turn_id", json!(turn)),
        ("parent_thread_id", json!(parent)),
        ("session_kind", session_kind),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect()
}

pub(super) fn memory_kind(
    body: &Value,
    headers: &BTreeMap<String, String>,
) -> Option<&'static str> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    match header("x-openai-subagent") {
        Some("memory_consolidation") => return Some("memory_consolidation"),
        Some("memory" | "memory_extraction") => return Some("memory_extraction"),
        _ => {}
    }
    let metadata = body["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .or_else(|| header("x-codex-turn-metadata"))
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_default();
    match metadata["request_kind"]
        .as_str()
        .or_else(|| metadata["kind"].as_str())
        .or_else(|| metadata["call_kind"].as_str())
    {
        Some("memory" | "memory_extraction") => Some("memory_extraction"),
        Some("memory_consolidation") => Some("memory_consolidation"),
        _ => None,
    }
}

/// Fixed vocabulary only; never record unknown user text.
pub(super) fn reasoning_effort(
    root: Option<&Value>,
    input: Option<&Value>,
    metadata: Option<&Value>,
) -> &'static str {
    let metadata = metadata
        .and_then(|metadata| metadata["x-codex-turn-metadata"].as_str())
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_default();
    // Lite pins root effort; turn metadata also covers unchanged incremental turns.
    let effort = metadata
        .get("reasoning_effort")
        .or_else(|| {
            input
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .rev()
                .filter(|item| item["type"] == "configuration_update")
                .find_map(|item| item.get("reasoning")?.get("effort"))
        })
        .or_else(|| root.and_then(|reasoning| reasoning.get("effort")));
    if let Some(effort) = effort {
        return match effort {
            Value::Null => "null",
            Value::String(value) => match value.as_str() {
                "none" => "none",
                "minimal" => "minimal",
                "low" => "low",
                "medium" => "medium",
                "high" => "high",
                "xhigh" => "xhigh",
                "max" => "max",
                "ultra" => "ultra",
                _ => "unknown",
            },
            _ => "invalid_type",
        };
    }
    match root {
        None | Some(Value::Object(_)) => "missing",
        Some(Value::Null) => "null",
        _ => "invalid_type",
    }
}
#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(super) fn request_bytes(body: &Value) -> usize {
    let mut counter = RequestJsonCounter::default();
    crate::util::spaced_json::write(&mut counter, body)
        .expect("JSON Value serialization is infallible");
    counter.serialized_bytes
}

#[derive(Default)]
struct RequestJsonCounter {
    serialized_bytes: usize,
}

impl Write for RequestJsonCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.serialized_bytes = self.serialized_bytes.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{reasoning_effort, request_bytes};
    use serde_json::{Value, json};

    #[test]
    fn effort_is_a_fact_with_no_default_or_content_retention() {
        for (body, expected) in [
            (json!({}), "missing"),
            (json!({"reasoning":null}), "null"),
            (json!({"reasoning":{"effort":null}}), "null"),
            (json!({"reasoning":{"effort":"none"}}), "none"),
            (json!({"reasoning":{"effort":"low"}}), "low"),
            (json!({"reasoning":{"effort":"medium"}}), "medium"),
            (
                json!({"reasoning":{"effort":"high"}, "input":[],
                    "client_metadata":{"x-codex-turn-metadata":"{\"reasoning_effort\":\"low\"}"}}),
                "low",
            ),
            (json!({"reasoning":{"effort":"private-effort"}}), "unknown"),
            (
                json!({"reasoning":{"effort":"high"},"input":[
                    {"type":"configuration_update","reasoning":{"effort":"high"}},
                    {"type":"configuration_update","reasoning":{"effort":"low"}},
                    {"type":"configuration_update","reasoning":{"summary":"auto"}}
                ]}),
                "low",
            ),
            (
                json!({"reasoning":{"effort":"high"},"input":[
                    {"type":"configuration_update","reasoning":{"effort":null}}
                ]}),
                "null",
            ),
            (
                json!({"reasoning":{"effort":{"secret":"private-value"}}}),
                "invalid_type",
            ),
        ] {
            assert_eq!(
                reasoning_effort(
                    body.get("reasoning"),
                    body.get("input"),
                    body.get("client_metadata")
                ),
                expected
            );
        }
    }

    #[test]
    fn request_bytes_and_tool_pairs_preserve_content_free_wire_facts() {
        let large = "quote \" slash \\ snow 雪, :".repeat(16_384);
        for body in [
            json!({"input":large}),
            json!({"input":[{"type":"tool_search_output","execution":"server","tools":[]}]}),
            json!({"input":[
                {"type":"tool_search_call","execution":"client","call_id":"s","arguments":{}},
                {"type":"tool_search_output","execution":"client","call_id":"s","tools":[]}
            ]}),
            json!({"input":[
                {"type":"tool_search_call","execution":"client","call_id":"s","arguments":{"query":"private-search"}},
                {"type":"tool_search_output","execution":"client","call_id":"s","tools":[]},
                {"type":"function_call","call_id":"f","name":"private-tool","arguments":"{}"},
                {"type":"function_call_output","call_id":"f","output":"private-output"}
            ]}),
        ] {
            let mut wire = Vec::new();
            crate::util::spaced_json::write(&mut wire, &body).unwrap();
            assert_eq!(request_bytes(&body), wire.len());
            let facts = super::facts(&body, &Default::default(), None);
            assert_eq!(
                facts["tool_pairing_status"],
                if body["input"].is_array() && body["input"][0]["execution"] != "server" {
                    "paired"
                } else {
                    "none"
                }
            );
            let encoded = Value::Object(facts).to_string();
            for private in ["private-search", "private-tool", "private-output"] {
                assert!(!encoded.contains(private));
            }
        }
    }
}
