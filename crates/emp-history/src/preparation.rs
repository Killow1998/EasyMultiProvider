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
    let replacement = normalize_tool_pairs(compaction_slice(&snapshot.items)?, &source)?;
    let mut input = source[..boundary].to_vec();
    input.extend(replacement.iter().filter_map(wire_item));
    let active_start = input.len();
    input.extend_from_slice(&source[boundary + 1..]);
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
    let input = match root.get("input") {
        Some(Value::Array(items)) => items.as_slice(),
        Some(item @ Value::Object(_)) => std::slice::from_ref(item),
        _ => &[],
    };
    let needs_preparation = input.iter().filter_map(Value::as_object).any(|item| {
        item.get("type").and_then(Value::as_str) == Some("compaction")
            && (!native_destination
                || portable_encrypted(item)
                    .is_some_and(|value| value.starts_with(COMPACTION_PREFIX)))
    });
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

pub fn build_compaction_replacement(
    items: &[VisibleItem],
) -> Result<Vec<VisibleItem>, HistoryError> {
    normalize_tool_pairs(compaction_slice(items)?, &[])
}

fn compaction_slice(items: &[VisibleItem]) -> Result<&[VisibleItem], HistoryError> {
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
    Ok(if has_summary {
        &items[boundary..=boundary]
    } else {
        &items[..boundary]
    })
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
