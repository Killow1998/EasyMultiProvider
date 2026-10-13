//! Pure whole-item boundaries. Borrow history; allocate only the current wire batch.
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::io::{self, Write};

// The live native endpoint rejected a 16.83 MiB message with 1009 even with
// permessage-deflate. Leave headroom; this is an application-message budget,
// not a token limit or a claim about encoded/TLS bytes.
pub(super) const MESSAGE_BUDGET: usize = 8 * 1024 * 1024;
const PARENT_RESERVE: usize = 2048;

pub(in crate::api::websocket::native_socket) struct Prefill<'a> {
    pub(super) base: Map<String, Value>,
    pub(super) chunks: Vec<&'a [Value]>,
}

#[derive(Default)]
struct ByteCount(usize);
impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn json_bytes(value: &Value) -> usize {
    let mut count = ByteCount::default();
    serde_json::to_writer(&mut count, value).expect("JSON value serialization");
    count.0
}

impl<'a> Prefill<'a> {
    pub(in crate::api::websocket::native_socket) fn plan(payload: &'a Value) -> Option<Self> {
        if payload
            .get("previous_response_id")
            .is_some_and(|id| !id.is_null())
            || json_bytes(payload) <= MESSAGE_BUDGET
        {
            return None;
        }
        let input = payload.get("input")?.as_array()?;
        if input.len() < 2 {
            return None;
        }
        let base: Map<_, _> = payload
            .as_object()?
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "input" | "previous_response_id"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let overhead = json_bytes(&Value::Object(base.clone())) + PARENT_RESERVE;
        let available = MESSAGE_BUDGET.checked_sub(overhead)?;
        let mut pending = BTreeSet::new();
        let mut group_size = 0;
        let mut chunk_size = 0;
        let mut group_start = 0;
        let mut boundaries = Vec::new();
        for (index, item) in input.iter().enumerate() {
            match item["type"].as_str().unwrap_or("message") {
                // Codex's server-owned search results have no client call/output
                // obligation. Preserve each opaque item without manufacturing a pair.
                "tool_search_call" | "tool_search_output" if item["execution"] == "server" => {}
                "web_search_call" | "image_generation_call" if item["status"] == "completed" => {}
                "function_call" | "custom_tool_call" | "tool_search_call" => {
                    if !pending.insert(item["call_id"].as_str()?) {
                        return None;
                    }
                }
                "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                    if !pending.remove(item["call_id"].as_str()?) {
                        return None;
                    }
                }
                // Unknown tool protocols may have dependencies we cannot infer.
                kind if kind.ends_with("_call") || kind.ends_with("_output") => return None,
                _ => {}
            }
            group_size += json_bytes(item) + 1;
            if !pending.is_empty() {
                continue;
            }
            if group_size > available {
                return None;
            }
            if chunk_size + group_size > available {
                boundaries.push(group_start);
                chunk_size = 0;
            }
            chunk_size += group_size;
            group_size = 0;
            group_start = index + 1;
        }
        if !pending.is_empty() || boundaries.is_empty() {
            return None;
        }
        boundaries.push(input.len());
        let mut start = 0;
        let chunks = boundaries
            .into_iter()
            .map(|end| {
                let chunk = &input[start..end];
                start = end;
                chunk
            })
            .collect();
        Some(Self { base, chunks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prefill_keeps_atomic_input_and_paired_tools_or_declines_without_truncation() {
        let huge = "x".repeat(MESSAGE_BUDGET / 2);
        let call = json!({"type":"function_call","call_id":"pair","name":"read","arguments":"{}"});
        let result = json!({"type":"function_call_output","call_id":"pair","output":"ok"});
        let payload = json!({"type":"response.create","instructions":"preserve exactly","input":[
            {"role":"user","content":huge},call,result,{"role":"user","content":huge}
        ]});
        let plan = Prefill::plan(&payload).unwrap();
        let restored: Vec<_> = plan.chunks.iter().flat_map(|c| c.iter().cloned()).collect();
        assert_eq!(json!(restored), payload["input"]);
        assert_eq!(plan.base["instructions"], payload["instructions"]);
        assert!(
            plan.chunks
                .iter()
                .any(|c| { c.windows(2).any(|p| p == [call.clone(), result.clone()]) })
        );
        let mut incremental = payload.clone();
        incremental["previous_response_id"] = json!("parent");
        assert!(Prefill::plan(&incremental).is_none());
        let one_item = json!({"input":[{"role":"user","content":"x".repeat(MESSAGE_BUDGET+1)}]});
        assert!(Prefill::plan(&one_item).is_none());

        let search = json!({"type":"tool_search_call","call_id":"search","execution":"client","arguments":{}});
        let found = json!({"type":"tool_search_output","call_id":"search","execution":"client","status":"completed","tools":[]});
        let with_search = json!({"input":[{"role":"user","content":huge},search,found,
            {"type":"web_search_call","status":"completed","action":{"type":"search","query":"synthetic"}},
            {"type":"tool_search_output","execution":"server","status":"completed","tools":[]},
            {"role":"user","content":huge}]});
        let plan = Prefill::plan(&with_search).unwrap();
        assert!(plan.chunks.iter().any(|chunk| {
            chunk
                .windows(2)
                .any(|pair| pair == [search.clone(), found.clone()])
        }));
        let mut unknown = with_search;
        unknown["input"][3]["type"] = json!("unknown_tool_call");
        assert!(Prefill::plan(&unknown).is_none());
    }
}
