//! Request-local checkpoint decoding and visible history preparation.
use crate::tool_pairs::normalize_tool_pairs;
use crate::wire::{content_text, message, wire_item};
use crate::{
    ACTIVE_INPUT_START, CHECKPOINT_PREFIX, COMPACTION_PREFIX, HistoryError, HistoryReader,
    VisibleItem, request_history_anchor,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde_json::{Map, Value};

pub fn prepare<R: HistoryReader + ?Sized>(
    body: &Value,
    incoming: &std::collections::BTreeMap<String, String>,
    native_destination: bool,
    reader: &R,
) -> Result<Value, HistoryError> {
    let root = history_projection_root(body)?;
    if has_previous_response(root) {
        return Ok(body.clone());
    }
    let decoded = decode_portable_items(root)?;
    if native_destination {
        return Ok(Value::Object(decoded));
    }
    let source = input_items(&decoded);
    let opaque = opaque_compaction_indexes(&source);
    if opaque.is_empty() {
        return Ok(Value::Object(decoded));
    }
    if opaque.len() != 1 {
        return Err(HistoryError::new("multiple_compaction_boundaries"));
    }
    let boundary = opaque[0];
    let compaction = source[boundary]
        .as_object()
        .ok_or_else(|| HistoryError::new("compaction_boundary_missing"))?;
    let anchor = request_history_anchor(&decoded, incoming)?;
    if anchor.thread_id.is_none() {
        return Err(HistoryError::new("thread_identity_missing"));
    }
    if anchor.turn_id.is_none() {
        return Err(HistoryError::new("turn_identity_missing"));
    }
    let snapshot = reader.read_compaction_history(&anchor, compaction)?;
    if Some(snapshot.thread_id.as_str()) != anchor.thread_id.as_deref() {
        return Err(HistoryError::new("thread_mismatch"));
    }
    let replacement = build_compaction_replacement(&snapshot.items)?;
    let history = replacement.iter().filter_map(wire_item).collect::<Vec<_>>();
    let trigger = source
        .last()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"));
    let mut tail = source[boundary + 1..].to_vec();
    if trigger.is_some() {
        tail.pop();
    }
    let mut input = source[..boundary].to_vec();
    input.extend(history.iter().cloned());
    let active_start = input.len();
    input.extend(tail);
    if let Some(trigger) = trigger {
        input.push(trigger.clone());
    }
    let mut projected = decoded;
    projected.insert("input".to_owned(), Value::Array(input));
    projected.insert(ACTIVE_INPUT_START.to_owned(), Value::from(active_start));
    Ok(Value::Object(projected))
}

/// Prepare an owned request while returning it untouched when history
/// reconstruction cannot change its input.
pub fn prepare_owned<R: HistoryReader + ?Sized>(
    body: Value,
    incoming: &std::collections::BTreeMap<String, String>,
    native_destination: bool,
    reader: &R,
) -> Result<Value, HistoryError> {
    let root = history_projection_root(&body)?;
    if has_previous_response(root) {
        return Ok(body);
    }
    let needs_preparation = if native_destination {
        contains_portable_compaction(root)
    } else {
        contains_compaction(root)
    };
    if !needs_preparation {
        return Ok(body);
    }
    prepare(&body, incoming, native_destination, reader)
}

fn history_projection_root(body: &Value) -> Result<&Map<String, Value>, HistoryError> {
    body.as_object()
        .ok_or_else(|| HistoryError::new("invalid_history_projection"))
}

fn has_previous_response(root: &Map<String, Value>) -> bool {
    root.get("previous_response_id")
        .is_some_and(|value| !value.is_null())
}

fn contains_compaction(root: &Map<String, Value>) -> bool {
    input_objects_match(root, |item| {
        item.get("type").and_then(Value::as_str) == Some("compaction")
    })
}

fn contains_portable_compaction(root: &Map<String, Value>) -> bool {
    input_objects_match(root, |item| {
        item.get("type").and_then(Value::as_str) == Some("compaction")
            && portable_encrypted(item).is_some_and(|value| value.starts_with(COMPACTION_PREFIX))
    })
}

fn input_objects_match(
    root: &Map<String, Value>,
    predicate: impl Fn(&Map<String, Value>) -> bool,
) -> bool {
    match root.get("input") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).any(predicate),
        Some(Value::Object(item)) => predicate(item),
        _ => false,
    }
}

pub fn build_compaction_replacement(
    items: &[VisibleItem],
) -> Result<Vec<VisibleItem>, HistoryError> {
    let boundary = items
        .iter()
        .rposition(|item| {
            matches!(
                item.kind.as_str(),
                "compaction_summary" | "compaction_marker"
            )
        })
        .ok_or_else(|| HistoryError::new("compaction_summary_missing"))?;
    let compacted = &items[boundary];
    let has_summary = content_text(&compacted.content).is_some_and(|text| !text.trim().is_empty());
    let replacement = if has_summary {
        vec![compacted.clone()]
    } else {
        items[..boundary].to_vec()
    };
    normalize_tool_pairs(replacement)
}

fn decode_portable_items(root: &Map<String, Value>) -> Result<Map<String, Value>, HistoryError> {
    let mut projected = root.clone();
    let mut source = input_items(root);
    let mut latest = None;
    for (index, item) in source.iter_mut().enumerate() {
        if item.get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        let Some(encoded) = item.get("encrypted_content").and_then(Value::as_str) else {
            continue;
        };
        let Some(encoded) = encoded.strip_prefix(COMPACTION_PREFIX) else {
            continue;
        };
        let bytes = URL_SAFE
            .decode(encoded)
            .map_err(|_| HistoryError::new("portable_checkpoint_encoding_invalid"))?;
        let summary = String::from_utf8(bytes)
            .map_err(|_| HistoryError::new("portable_checkpoint_utf8_invalid"))?;
        if summary.trim().is_empty() {
            return Err(HistoryError::new("portable_checkpoint_empty"));
        }
        *item = message("user", &format!("{CHECKPOINT_PREFIX}\n\n{summary}"));
        latest = Some(index);
    }
    if root.get("input").is_some_and(Value::is_array) || latest.is_some() {
        projected.insert("input".to_owned(), Value::Array(source));
    }
    if let Some(index) = latest {
        projected.insert(ACTIVE_INPUT_START.to_owned(), Value::from(index + 1));
    }
    Ok(projected)
}

fn opaque_compaction_indexes(source: &[Value]) -> Vec<usize> {
    source
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.get("type").and_then(Value::as_str) == Some("compaction")
                && item
                    .as_object()
                    .and_then(portable_encrypted)
                    .is_none_or(|value| !value.starts_with(COMPACTION_PREFIX)))
            .then_some(index)
        })
        .collect()
}

fn portable_encrypted(item: &Map<String, Value>) -> Option<&str> {
    item.get("encrypted_content").and_then(Value::as_str)
}
fn input_items(root: &Map<String, Value>) -> Vec<Value> {
    match root.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(item)) => vec![Value::Object(item.clone())],
        Some(Value::String(value)) => vec![Value::String(value.clone())],
        _ => Vec::new(),
    }
}
