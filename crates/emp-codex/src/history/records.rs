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

pub(super) fn parse_records(bytes: &[u8]) -> Result<Vec<Record>, HistoryError> {
    let mut records = Vec::new();
    let lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    for (line, raw) in lines.iter().enumerate() {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(raw) {
            Ok(Value::Object(value)) => records.push(Record { value }),
            Ok(_) => return Err(HistoryError::new("invalid_json")),
            Err(error) if line + 1 == lines.len() && !bytes.ends_with(b"\n") && error.is_eof() => {}
            Err(_) => return Err(HistoryError::new("invalid_json")),
        }
    }
    Ok(records)
}

pub(super) fn validate_thread(
    records: &[Record],
    expected: Option<&str>,
) -> Result<(), HistoryError> {
    let ids = records
        .iter()
        .filter(|record| {
            matches!(
                token(record.value.get("type")).as_str(),
                "session_meta" | "sessionmeta"
            )
        })
        .filter_map(|record| {
            let payload = record.value.get("payload").and_then(Value::as_object);
            string_from(
                payload.unwrap_or(&record.value),
                &["id", "thread_id", "threadId", "session_id", "sessionId"],
            )
        })
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(HistoryError::new("session_meta_missing"));
    }
    if ids.iter().any(|id| Some(id.as_str()) != expected) {
        return Err(HistoryError::new("thread_mismatch"));
    }
    if ids.iter().collect::<BTreeSet<_>>().len() != 1 {
        return Err(HistoryError::new("thread_identity_conflict"));
    }
    Ok(())
}

pub(super) fn anchor_boundary(
    records: &[Record],
    turn: Option<&str>,
) -> Result<usize, HistoryError> {
    let Some(turn) = turn else {
        return Ok(records.len());
    };
    if let Some(index) = records
        .iter()
        .position(|record| record_turn_id(&record.value).as_deref() == Some(turn))
    {
        return Ok(index);
    }
    let explicit = records
        .iter()
        .filter_map(|record| record_turn_id(&record.value))
        .collect::<BTreeSet<_>>();
    let terminal = records
        .iter()
        .filter_map(|record| {
            let payload = record.value.get("payload").and_then(Value::as_object)?;
            (token(record.value.get("type")) == "event_msg"
                && token(payload.get("type")) == "task_complete")
                .then(|| record_turn_id(&record.value))
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    if explicit.is_subset(&terminal) {
        Ok(records.len())
    } else {
        Err(HistoryError::new("turn_not_found"))
    }
}

pub(super) fn exact_compaction_boundary(
    records: &[Record],
    compaction: &Map<String, Value>,
) -> Result<usize, HistoryError> {
    let encoded = compaction
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HistoryError::new("compaction_identity_missing"))?;
    let matches = records
        .iter()
        .enumerate()
        .filter_map(|(index, record)| {
            replacement_history(&record.value)
                .ok()
                .flatten()
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item.get("type").and_then(Value::as_str) == Some("compaction")
                            && item.get("encrypted_content").and_then(Value::as_str)
                                == Some(encoded)
                    })
                })
                .then_some(index)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [index] => Ok(index + 1),
        [] => Err(HistoryError::new("compaction_identity_missing")),
        _ => Err(HistoryError::new("compaction_identity_ambiguous")),
    }
}

pub(super) fn successful_turns(records: &[Record]) -> BTreeSet<String> {
    let mut outcomes = BTreeMap::new();
    for record in records {
        let Some(payload) = record.value.get("payload").and_then(Value::as_object) else {
            continue;
        };
        if token(record.value.get("type")) != "event_msg"
            || token(payload.get("type")) != "task_complete"
        {
            continue;
        }
        if let Some(turn) = record_turn_id(&record.value) {
            outcomes.insert(turn, payload.get("error").is_none_or(Value::is_null));
        }
    }
    outcomes
        .into_iter()
        .filter_map(|(turn, success)| success.then_some(turn))
        .collect()
}

pub(super) fn record_turn_ids(records: &[Record]) -> Vec<Option<String>> {
    let mut active = None;
    records
        .iter()
        .map(|record| {
            let explicit = record_turn_id(&record.value);
            if explicit.is_some() {
                active = explicit.clone();
            }
            explicit.or_else(|| active.clone())
        })
        .collect()
}

pub(super) fn source_model(records: &[Record], incoming_turn: Option<&str>) -> Option<String> {
    let successful = successful_turns(records);
    records
        .iter()
        .filter_map(|record| {
            if !matches!(
                token(record.value.get("type")).as_str(),
                "turn_context" | "turncontext"
            ) {
                return None;
            }
            let payload = record
                .value
                .get("payload")
                .and_then(Value::as_object)
                .unwrap_or(&record.value);
            let turn = record_turn_id(&record.value)?;
            successful
                .contains(&turn)
                .then(|| string_from(payload, &["model", "model_id", "modelId", "selected_model"]))
                .flatten()
        })
        .next_back()
        .or_else(|| incoming_turn.is_none().then_some(None).flatten())
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
