//! Native history projection. Opaque native state and unknown fields survive;
//! only complete foreign tool pairs, EMP summaries, and plaintext reasoning
//! receive the same transformations as Python's `dialects.project_request`.

use crate::portable_responses::{
    COMPACTION_PREFIX, COMPACTION_SUMMARY_PREFIX, PortableProjectionError, decode_compaction,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeProjectionError {
    Projection(PortableProjectionError),
    /// Python raises TypeError before projection when a type cannot be hashed.
    /// The HTTP owner must preserve its generic 500 boundary, not turn it into
    /// a retryable protocol error or silently forward the malformed history.
    UnhashableItemType,
}

impl From<PortableProjectionError> for NativeProjectionError {
    fn from(error: PortableProjectionError) -> Self {
        Self::Projection(error)
    }
}

impl fmt::Display for NativeProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Projection(error) => error.fmt(formatter),
            Self::UnhashableItemType => {
                formatter.write_str("native input item type is not hashable")
            }
        }
    }
}

impl std::error::Error for NativeProjectionError {}

fn foreign_tool_indices(source: &[Value]) -> Result<BTreeSet<usize>, NativeProjectionError> {
    let mut pairs: BTreeMap<&str, Vec<(usize, &str)>> = BTreeMap::new();
    let mut foreign = Vec::new();
    for (index, item) in source.iter().enumerate() {
        if matches!(item.get("type"), Some(Value::Array(_) | Value::Object(_))) {
            return Err(NativeProjectionError::UnhashableItemType);
        }
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let prefix = match kind {
            "function_call" => Some("fc"),
            "custom_tool_call" => Some("ctc"),
            "function_call_output" | "custom_tool_call_output" => None,
            _ => continue,
        };
        let call_id = item.get("call_id").and_then(Value::as_str);
        if let Some(call_id) = call_id.filter(|id| !id.is_empty()) {
            pairs.entry(call_id).or_default().push((index, kind));
        }
        if let (Some(prefix), Some(id)) = (prefix, item.get("id"))
            && !id.is_null()
            && !id.as_str().is_some_and(|id| id.starts_with(prefix))
        {
            foreign.push((index, kind, call_id));
        }
    }
    let mut stripped = BTreeSet::new();
    for (index, kind, call_id) in foreign {
        let pair = call_id.and_then(|id| pairs.get(id));
        match pair.map(Vec::as_slice) {
            Some([(call_index, call_kind), (output_index, output_kind)])
                if *call_index == index
                    && *call_kind == kind
                    && *output_kind == format!("{kind}_output") =>
            {
                stripped.extend([index, *output_index]);
            }
            _ => {
                return Err(PortableProjectionError::new(
                    index,
                    kind,
                    Vec::new(),
                    "incompatible_tool_history",
                )
                .into());
            }
        }
    }
    Ok(stripped)
}

fn native_input(source: &Value) -> Result<Value, NativeProjectionError> {
    let Some(items) = source.as_array() else {
        return Ok(source.clone());
    };
    let foreign = foreign_tool_indices(items)?;
    let mut result = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if foreign.contains(&index) {
            let mut projected = item
                .as_object()
                .expect("validated foreign tool item")
                .clone();
            projected.remove("id");
            result.push(Value::Object(projected));
            continue;
        }
        let kind = item.get("type").and_then(Value::as_str);
        if kind == Some("compaction")
            && let Some(encoded) = item.get("encrypted_content").and_then(Value::as_str)
            && encoded.starts_with(COMPACTION_PREFIX)
        {
            let summary = decode_compaction(encoded).ok_or_else(|| {
                PortableProjectionError::new(index, "compaction", Vec::new(), "invalid_compaction")
            })?;
            result.push(json!({"type": "message", "role": "user", "content": [{
                "type": "input_text", "text": format!("{COMPACTION_SUMMARY_PREFIX}\n\n{summary}")
            }]}));
        } else if kind == Some("reasoning") {
            if item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
            {
                let mut opaque = item.as_object().expect("reasoning object").clone();
                for field in ["content", "text", "reasoning_text", "thinking"] {
                    opaque.remove(field);
                }
                result.push(Value::Object(opaque));
            }
        } else {
            result.push(item.clone());
        }
    }
    Ok(Value::Array(result))
}

/// Project a native body without resolving its model or collaboration namespace.
/// Unlike portable Responses, native incremental IDs and extension fields survive.
pub fn project_request(body: &Map<String, Value>) -> Result<Value, NativeProjectionError> {
    let mut projected = body.clone();
    projected.insert(
        "input".to_owned(),
        native_input(body.get("input").unwrap_or(&Value::Null))?,
    );
    Ok(Value::Object(projected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_projection_preserves_opaque_state_and_pairing() {
        let body = json!({"previous_response_id": "resp_native", "input": [
            {"type": "reasoning", "text": "plaintext"},
            {"type": "reasoning", "encrypted_content": "opaque", "content": [], "id": "rs_native"},
            {"type": "function_call", "id": "foreign", "call_id": "pair", "arguments": "{}"},
            {"type": "function_call_output", "id": "foreign_output", "call_id": "pair", "output": "result"}
        ]});
        let projected = project_request(body.as_object().unwrap()).unwrap();
        assert_eq!(projected["previous_response_id"], "resp_native");
        assert_eq!(
            projected["input"][0],
            json!({"type": "reasoning", "encrypted_content": "opaque", "id": "rs_native"})
        );
        assert!(projected["input"][1].get("id").is_none());
        assert!(projected["input"][2].get("id").is_none());
        assert_eq!(
            projected["input"][1]["call_id"],
            projected["input"][2]["call_id"]
        );
        assert_eq!(
            project_request(projected.as_object().unwrap()).unwrap(),
            projected
        );
        assert_eq!(body["input"][2]["id"], "foreign");
    }
}
