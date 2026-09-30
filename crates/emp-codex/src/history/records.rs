use super::MAX_ROLLOUT_SCAN_BYTES;
use super::location::Location;
use super::visible::{content_text, normalize_visible_item, string_from, token, uuid_shape};
use emp_history::{HistoryError, VisibleItem};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(super) fn locate(database: &Path, thread: &str) -> Result<Location, HistoryError> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| HistoryError::new("database_unavailable"))?;
    let columns = connection
        .prepare("PRAGMA table_info(threads)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<BTreeSet<_>, _>>()
        })
        .map_err(|_| HistoryError::new("database_unavailable"))?;
    if !columns.contains("id") || !columns.contains("rollout_path") {
        return Err(HistoryError::new("threads_schema_unsupported"));
    }
    let mode = if columns.contains("history_mode") {
        "history_mode"
    } else {
        "'legacy'"
    };
    let model = if columns.contains("model") {
        "model"
    } else {
        "NULL"
    };
    let query = format!(
        "SELECT id, rollout_path, {mode} AS history_mode, {model} AS model FROM threads WHERE id = ?1 LIMIT 2"
    );
    let mut statement = connection
        .prepare(&query)
        .map_err(|_| HistoryError::new("database_unavailable"))?;
    let rows = statement
        .query_map([thread], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|_| HistoryError::new("database_unavailable"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| HistoryError::new("database_unavailable"))?;
    if rows.is_empty() {
        return Err(HistoryError::new("thread_missing"));
    }
    if rows.len() != 1 {
        return Err(HistoryError::new("thread_identity_conflict"));
    }
    let (raw_path, mode, model) = rows.into_iter().next().expect("one row");
    if raw_path.trim().is_empty() {
        return Err(HistoryError::new("rollout_path_missing"));
    }
    let mode = mode.unwrap_or_else(|| "legacy".to_owned());
    if !matches!(mode.as_str(), "legacy" | "paginated") {
        return Err(HistoryError::new("invalid_history_mode"));
    }
    let path = PathBuf::from(raw_path.trim());
    Ok(Location {
        path: if path.is_absolute() {
            path
        } else {
            database
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path)
        },
        mode,
        source_model: model.filter(|value| !value.trim().is_empty() && value.len() <= 512),
    })
}

pub(super) fn session_meta_id(record: &Map<String, Value>) -> Option<String> {
    let payload = record.get("payload").and_then(Value::as_object);
    string_from(
        payload.unwrap_or(record),
        &["id", "thread_id", "threadId", "session_id", "sessionId"],
    )
}

pub(super) fn session_meta_history_mode(
    record: &Map<String, Value>,
) -> Result<Option<String>, HistoryError> {
    let payload = record.get("payload").and_then(Value::as_object);
    let mode = payload.unwrap_or(record).get("history_mode");
    match mode {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(mode)) if matches!(mode.as_str(), "legacy" | "paginated") => {
            Ok(Some(mode.clone()))
        }
        Some(_) => Err(HistoryError::new("invalid_history_mode")),
    }
}

/// Lineage pointer recorded on a paginated rollout's session meta.
///
/// Returns `(parent rollout id, end_ordinal_exclusive, end_byte_offset)` when
/// the child inherits a bounded prefix from another rollout. The byte offset
/// freezes the physical prefix the child forked from; `0` (or absent) keeps
/// the ordinal bound as the only cutoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HistoryBase {
    pub(super) thread_id: String,
    pub(super) end_ordinal_exclusive: u64,
    pub(super) end_byte_offset: u64,
}

pub(super) fn session_meta_history_base(
    record: &Map<String, Value>,
) -> Result<Option<HistoryBase>, HistoryError> {
    let Some(payload) = record.get("payload").and_then(Value::as_object) else {
        return Ok(None);
    };
    let Some(raw_base) = payload.get("history_base") else {
        return Ok(None);
    };
    if raw_base.is_null() {
        return Ok(None);
    }
    let base = raw_base
        .as_object()
        .ok_or_else(|| HistoryError::new("invalid_history_base"))?;
    let thread_id = string_from(base, &["thread_id", "threadId", "rollout_id", "rolloutId"])
        .filter(|value| uuid_shape(value))
        .ok_or_else(|| HistoryError::new("invalid_history_base"))?;
    let end_ordinal_exclusive = base
        .get("end_ordinal_exclusive")
        .or_else(|| base.get("endOrdinalExclusive"))
        .and_then(Value::as_u64)
        .ok_or_else(|| HistoryError::new("invalid_history_base"))?;
    let end_byte_offset = match base
        .get("end_byte_offset")
        .or_else(|| base.get("endByteOffset"))
    {
        Some(value) => value
            .as_u64()
            .ok_or_else(|| HistoryError::new("invalid_history_base"))?,
        None => 0,
    };
    if end_byte_offset > MAX_ROLLOUT_SCAN_BYTES {
        return Err(HistoryError::new("invalid_history_base"));
    }
    Ok(Some(HistoryBase {
        thread_id,
        end_ordinal_exclusive,
        end_byte_offset,
    }))
}

pub(super) fn replacement_contains_encoded(record: &Map<String, Value>, encoded: &str) -> bool {
    if !matches!(
        token(record.get("type")).as_str(),
        "compaction" | "compaction_summary" | "compacted" | "compacted_summary"
    ) {
        return false;
    }
    let payload = record
        .get("payload")
        .and_then(Value::as_object)
        .unwrap_or(record);
    payload
        .get("replacement_history")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("compaction")
                    && item.get("encrypted_content").and_then(Value::as_str) == Some(encoded)
            })
        })
}

pub(super) fn replacement_history(
    record: &Map<String, Value>,
) -> Result<Option<Vec<Map<String, Value>>>, HistoryError> {
    if !matches!(
        token(record.get("type")).as_str(),
        "compaction" | "compaction_summary" | "compacted" | "compacted_summary"
    ) {
        return Ok(None);
    }
    let payload = record
        .get("payload")
        .and_then(Value::as_object)
        .unwrap_or(record);
    let Some(value) = payload.get("replacement_history") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let items = value
        .as_array()
        .ok_or_else(|| HistoryError::new("invalid_replacement_history"))?;
    let items = items
        .iter()
        .map(|item| {
            item.as_object()
                .cloned()
                .ok_or_else(|| HistoryError::new("invalid_replacement_history"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Per-item metadata is positional: anything other than a parallel array
    // means the replacement cannot be trusted item for item.
    if let Some(metadata) = payload
        .get("replacement_history_metadata")
        .filter(|value| !value.is_null())
        && metadata
            .as_array()
            .is_none_or(|metadata| metadata.len() != items.len())
    {
        return Err(HistoryError::new("invalid_replacement_history_metadata"));
    }
    Ok(Some(items))
}

pub(super) fn replacement_entries(
    replacement: Vec<Map<String, Value>>,
    previous: &[VisibleItem],
    turn: Option<String>,
) -> Result<Vec<VisibleItem>, HistoryError> {
    let mut output = Vec::new();
    for raw in replacement {
        if raw.get("type").and_then(Value::as_str) == Some("compaction")
            && raw
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        {
            let boundary = previous.iter().rposition(|item| {
                matches!(
                    item.kind.as_str(),
                    "compaction_summary" | "compaction_marker"
                )
            });
            let portable = match boundary {
                Some(index) if content_text(&previous[index].content).trim().is_empty() => previous
                    .iter()
                    .enumerate()
                    .filter(|(position, _)| *position != index)
                    .map(|(_, item)| item.clone())
                    .collect(),
                Some(index) => previous[index..].to_vec(),
                None => previous.to_vec(),
            };
            if portable.is_empty() {
                return Err(HistoryError::new("opaque_replacement_unavailable"));
            }
            output.extend(portable);
        } else if let Some(item) = normalize_visible_item(&raw, turn.clone()) {
            output.push(item);
        }
    }
    let mut marker = VisibleItem::new("compaction_marker", Value::String(String::new()));
    marker.turn_id = turn;
    output.push(marker);
    Ok(output)
}

/// Window number persisted on a compaction record, when present.
pub(super) fn payload_window_number(record: &Map<String, Value>) -> Option<u64> {
    let payload = record.get("payload").and_then(Value::as_object);
    payload
        .unwrap_or(record)
        .get("window_number")
        .and_then(Value::as_u64)
}

/// Resume metadata contract marker on a compaction record.
pub(super) fn payload_resume_metadata(record: &Map<String, Value>) -> Option<&Value> {
    let payload = record.get("payload").and_then(Value::as_object);
    payload
        .unwrap_or(record)
        .get("resume_metadata")
        .filter(|value| !value.is_null())
}
