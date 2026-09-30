//! Validated thread, turn and window anchors from Codex request metadata.
use crate::{HistoryAnchor, HistoryError};
use serde_json::{Map, Value};

pub fn request_history_anchor(
    body: &Map<String, Value>,
    incoming: &std::collections::BTreeMap<String, String>,
) -> Result<HistoryAnchor, HistoryError> {
    let body_metadata = body
        .get("client_metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("x-codex-turn-metadata"));
    let raw_metadata = match body_metadata {
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(HistoryError::new("invalid_turn_metadata")),
        None => header(incoming, "x-codex-turn-metadata"),
    };
    let metadata = match raw_metadata {
        Some(value) if !value.trim().is_empty() => serde_json::from_str::<Value>(value)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| HistoryError::new("invalid_turn_metadata"))?,
        _ => Map::new(),
    };
    let header_thread = header(incoming, "thread-id").map(str::to_owned);
    let metadata_thread = string_alias(&metadata, "thread_id", "threadId")?;
    if let (Some(left), Some(right)) = (&header_thread, &metadata_thread)
        && left != right
    {
        return Err(HistoryError::new("conflicting_thread_identity"));
    }
    let metadata_window = string_alias(&metadata, "window_id", "windowId")?;
    let header_window = if body_metadata.is_some() && metadata_window.is_some() {
        None
    } else {
        header(incoming, "x-codex-window-id").map(str::to_owned)
    };
    if let (Some(left), Some(right)) = (&header_window, &metadata_window)
        && left != right
    {
        return Err(HistoryError::new("conflicting_window_identity"));
    }
    let legacy_session = header(incoming, "version")
        .filter(|version| legacy_codex_version(version))
        .and_then(|_| header(incoming, "session-id"))
        .map(str::to_owned);
    Ok(HistoryAnchor {
        thread_id: header_thread.or(metadata_thread).or(legacy_session),
        turn_id: if metadata
            .get("turn_id")
            .or_else(|| metadata.get("turnId"))
            .is_some_and(|v| v == "")
        {
            None
        } else {
            string_alias(&metadata, "turn_id", "turnId")?
        },
        window_id: header_window.or(metadata_window),
        forked_from_thread_id: optional_string(&metadata, "forked_from_thread_id")?,
    })
}
fn header<'a>(
    incoming: &'a std::collections::BTreeMap<String, String>,
    wanted: &str,
) -> Option<&'a str> {
    incoming
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.trim())
}

fn string_alias(
    value: &Map<String, Value>,
    snake: &str,
    camel: &str,
) -> Result<Option<String>, HistoryError> {
    optional_string(value, snake)?
        .map_or_else(|| optional_string(value, camel), |value| Ok(Some(value)))
}

fn optional_string(value: &Map<String, Value>, key: &str) -> Result<Option<String>, HistoryError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() && value.len() <= 512 => {
            Ok(Some(value.trim().to_owned()))
        }
        Some(_) => Err(HistoryError::new(format!("invalid_{key}"))),
    }
}

fn legacy_codex_version(value: &str) -> bool {
    let parts = value
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    matches!(parts.as_deref(), Ok([0, minor, patch]) if (*minor, *patch) <= (154, 0))
}
