//! Content-free request facts. Names, arguments, text and attachments are omitted.
use serde_json::{Value, json};
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
    result["parent_thread_id"] = json!(
        header("x-codex-parent-thread-id")
            .map(str::to_owned)
            .or_else(|| text(&metadata["forked_from_thread_id"]))
    );
    result
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
    use super::request_bytes;
    use serde_json::{Value, json};

    fn materialized_reference(body: &Value) -> usize {
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
            assert_eq!(request_bytes(&body), materialized_reference(&body));
            let facts = super::facts(&body, &Default::default());
            assert_eq!(
                facts["tool_pairing_status"],
                if body["input"].is_array() && body["input"][0]["execution"] != "server" {
                    "paired"
                } else {
                    "none"
                }
            );
            let encoded = facts.to_string();
            for private in ["private-search", "private-tool", "private-output"] {
                assert!(!encoded.contains(private));
            }
        }
    }
}
