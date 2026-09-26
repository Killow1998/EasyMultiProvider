use super::*;

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

/// Lineage pointer recorded on a paginated rollout's session meta.
///
/// Returns `(parent rollout id, end_ordinal_exclusive)` when the child
/// inherits a bounded prefix from another rollout.
pub(super) fn session_meta_history_base(record: &Map<String, Value>) -> Option<(String, u64)> {
    let payload = record.get("payload").and_then(Value::as_object)?;
    let base = payload.get("history_base")?.as_object()?;
    let thread_id = string_from(base, &["thread_id", "threadId", "rollout_id", "rolloutId"])?;
    let end_ordinal_exclusive = base
        .get("end_ordinal_exclusive")
        .or_else(|| base.get("endOrdinalExclusive"))
        .and_then(Value::as_u64)?;
    Some((thread_id, end_ordinal_exclusive))
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
    items
        .iter()
        .map(|item| {
            item.as_object()
                .cloned()
                .ok_or_else(|| HistoryError::new("invalid_replacement_history"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
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
