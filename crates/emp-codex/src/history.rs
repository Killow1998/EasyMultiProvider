//! Bounded, read-only reconstruction of Codex-visible rollout history.

use emp_history::{HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_ROLLOUT_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Record {
    value: Map<String, Value>,
}

#[derive(Clone, Debug)]
struct Location {
    path: PathBuf,
    mode: String,
    source_model: Option<String>,
}

pub struct CodexHomeHistoryReader {
    home: PathBuf,
}

impl CodexHomeHistoryReader {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn read_visible_history(
        &self,
        anchor: &HistoryAnchor,
    ) -> Result<HistorySnapshot, HistoryError> {
        let thread = anchor
            .thread_id
            .as_deref()
            .ok_or_else(|| HistoryError::new("thread_identity_missing"))?;
        let database = self.latest_state_database()?;
        let location = locate(&database, thread)?;
        self.read_rollout(anchor, &location, None)
    }

    fn latest_state_database(&self) -> Result<PathBuf, HistoryError> {
        let mut candidates = fs::read_dir(&self.home)
            .map_err(|_| HistoryError::new("state_database_missing"))?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                let name = path.file_stem()?.to_str()?;
                let version = name.strip_prefix("state_")?.parse::<u64>().ok()?;
                path.is_file().then_some((version, path))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(version, _)| *version);
        candidates
            .pop()
            .map(|(_, path)| path)
            .ok_or_else(|| HistoryError::new("state_database_missing"))
    }

    fn read_rollout(
        &self,
        anchor: &HistoryAnchor,
        location: &Location,
        exact_compaction: Option<&Map<String, Value>>,
    ) -> Result<HistorySnapshot, HistoryError> {
        let home = self
            .home
            .canonicalize()
            .map_err(|_| HistoryError::new("history_unavailable"))?;
        let path = location
            .path
            .canonicalize()
            .map_err(|_| HistoryError::new("source_missing"))?;
        if !path.starts_with(&home) {
            return Err(HistoryError::new("rollout_outside_codex_home"));
        }
        let before = fs::metadata(&path).map_err(|_| HistoryError::new("source_missing"))?;
        if !before.is_file() {
            return Err(HistoryError::new("source_missing"));
        }
        if before.len() > MAX_ROLLOUT_BYTES {
            return Err(HistoryError::new("source_too_large"));
        }
        let bytes = fs::read(&path).map_err(|_| HistoryError::new("source_unavailable"))?;
        let after = fs::metadata(&path).map_err(|_| HistoryError::new("source_changed"))?;
        if bytes.len() as u64 != before.len() || before.len() != after.len() {
            return Err(HistoryError::new("source_changed"));
        }
        let records = parse_records(&bytes)?;
        validate_thread(&records, anchor.thread_id.as_deref())?;
        let boundary = if let Some(compaction) = exact_compaction {
            exact_compaction_boundary(&records, compaction)?
        } else {
            anchor_boundary(&records, anchor.turn_id.as_deref())?
        };
        let history = &records[..boundary];
        let turn_ids = record_turn_ids(history);
        let mut successful = successful_turns(history);
        if exact_compaction.is_some()
            && let Some(turn) = turn_ids.last().and_then(Clone::clone)
        {
            successful.insert(turn);
        }
        let response_roles = history
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                (token(record.value.get("type")) == "response_item")
                    .then(|| message_role(&record.value))
                    .flatten()
                    .map(|role| (turn_ids[index].clone(), role))
            })
            .collect::<BTreeSet<_>>();
        let mut visible = Vec::<VisibleItem>::new();
        for (index, record) in history.iter().enumerate() {
            let turn = turn_ids[index].clone();
            if turn.as_ref().is_some_and(|turn| !successful.contains(turn)) {
                continue;
            }
            if token(record.value.get("type")) == "event_msg"
                && message_role(&record.value)
                    .is_some_and(|role| response_roles.contains(&(turn.clone(), role)))
            {
                continue;
            }
            if let Some(replacement) = replacement_history(&record.value)? {
                visible = replacement_entries(replacement, &visible, turn.clone())?;
                continue;
            }
            let Some(raw) = visible_payload(&record.value) else {
                continue;
            };
            if let Some(item) = normalize_visible_item(&raw, turn.clone()) {
                visible.push(item);
            }
        }
        if location.mode == "paginated"
            && history
                .iter()
                .all(|record| ordinal(&record.value).is_none())
        {
            return Err(HistoryError::new("ordinal_missing"));
        }
        Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().unwrap_or_default(),
            items: visible,
            source_model: source_model(history, anchor.turn_id.as_deref()).or_else(|| {
                anchor
                    .turn_id
                    .is_none()
                    .then(|| location.source_model.clone())
                    .flatten()
            }),
        })
    }
}

impl HistoryReader for CodexHomeHistoryReader {
    fn read_compaction_history(
        &self,
        anchor: &HistoryAnchor,
        compaction: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError> {
        match self.read_visible_history(anchor) {
            Ok(snapshot) => return Ok(snapshot),
            Err(error)
                if !matches!(error.reason(), "thread_missing" | "thread_mismatch")
                    || anchor.forked_from_thread_id.is_none() =>
            {
                return Err(error);
            }
            Err(_) => {}
        }
        let parent = anchor
            .forked_from_thread_id
            .as_deref()
            .ok_or_else(|| HistoryError::new("thread_missing"))?;
        if parent == anchor.thread_id.as_deref().unwrap_or_default() || !uuid_shape(parent) {
            return Err(HistoryError::new("fork_parent_invalid"));
        }
        let database = self.latest_state_database()?;
        let location = locate(&database, parent)?;
        let parent_anchor = HistoryAnchor {
            thread_id: Some(parent.to_owned()),
            ..HistoryAnchor::default()
        };
        let mut snapshot = self.read_rollout(&parent_anchor, &location, Some(compaction))?;
        snapshot.thread_id = anchor.thread_id.clone().unwrap_or_default();
        Ok(snapshot)
    }
}

fn locate(database: &Path, thread: &str) -> Result<Location, HistoryError> {
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

fn parse_records(bytes: &[u8]) -> Result<Vec<Record>, HistoryError> {
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

fn validate_thread(records: &[Record], expected: Option<&str>) -> Result<(), HistoryError> {
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

fn anchor_boundary(records: &[Record], turn: Option<&str>) -> Result<usize, HistoryError> {
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

fn exact_compaction_boundary(
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

fn successful_turns(records: &[Record]) -> BTreeSet<String> {
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

fn record_turn_ids(records: &[Record]) -> Vec<Option<String>> {
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

fn source_model(records: &[Record], incoming_turn: Option<&str>) -> Option<String> {
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

fn replacement_history(
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

fn replacement_entries(
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

fn visible_payload(record: &Map<String, Value>) -> Option<Map<String, Value>> {
    let kind = token(record.get("type"));
    let payload = record.get("payload").and_then(Value::as_object);
    if matches!(
        kind.as_str(),
        "session_meta" | "sessionmeta" | "turn_context" | "turncontext"
    ) {
        return payload.cloned();
    }
    if kind == "response_item" {
        return payload.cloned();
    }
    if kind == "event_msg" {
        let payload = payload?;
        let event = token(payload.get("type"));
        if matches!(event.as_str(), "user_message" | "usermessage") {
            return Some(Map::from_iter([
                ("type".to_owned(), json!("user_message")),
                (
                    "content".to_owned(),
                    json!({"text":payload.get("message"),"images":payload.get("images")}),
                ),
            ]));
        }
        if matches!(
            event.as_str(),
            "assistant_message" | "assistantmessage" | "agent_message" | "agentmessage"
        ) {
            return Some(Map::from_iter([
                ("type".to_owned(), json!("assistant_message")),
                (
                    "message".to_owned(),
                    payload.get("message").cloned().unwrap_or(Value::Null),
                ),
            ]));
        }
        return (!event.is_empty()).then(|| payload.clone());
    }
    if matches!(
        kind.as_str(),
        "compaction"
            | "compaction_summary"
            | "compacted"
            | "compacted_summary"
            | "compaction_marker"
            | "compaction_boundary"
    ) {
        return Some(
            payload
                .cloned()
                .unwrap_or_else(|| record.clone())
                .into_iter()
                .chain(std::iter::once(("type".to_owned(), Value::String(kind))))
                .collect(),
        );
    }
    record
        .get("item")
        .and_then(Value::as_object)
        .cloned()
        .or_else(|| record.get("type").is_some().then(|| record.clone()))
}

fn normalize_visible_item(raw: &Map<String, Value>, turn: Option<String>) -> Option<VisibleItem> {
    if raw.get("visible") == Some(&Value::Bool(false))
        || raw.get("hidden") == Some(&Value::Bool(true))
    {
        return None;
    }
    let raw_type = raw.get("type")?.as_str()?.to_owned();
    let kind = token(raw.get("type"));
    if matches!(
        kind.as_str(),
        "analysis"
            | "chain_of_thought"
            | "hidden_reasoning"
            | "hidden_cot"
            | "reasoning"
            | "thinking"
    ) {
        return None;
    }
    let (visible_kind, content) = match kind.as_str() {
        "message" => match raw.get("role").and_then(Value::as_str) {
            Some("user") => (
                "user_message",
                first_value(raw, &["content", "text", "message"]),
            ),
            Some("assistant") => (
                "assistant_message",
                first_value(raw, &["content", "text", "message"]),
            ),
            _ => return None,
        },
        "user_message" | "usermessage" => (
            "user_message",
            first_value(raw, &["content", "message", "text", "images"]),
        ),
        "assistant_message" | "assistantmessage" | "agent_message" | "agentmessage" => (
            "assistant_message",
            first_value(raw, &["content", "message", "text"]),
        ),
        "function_call" | "custom_tool_call" | "tool_call" | "tool_search_call" => (
            "tool_call",
            selected_map(
                raw,
                &[
                    "name",
                    "namespace",
                    "arguments",
                    "input",
                    "tool",
                    "status",
                    "execution",
                ],
            ),
        ),
        "function_call_output"
        | "custom_tool_call_output"
        | "tool_search_output"
        | "tool_result"
        | "tool_output"
        | "mcp_tool_result" => {
            let standalone = kind == "function_call_output"
                && string_from(raw, &["call_id", "callId"]).is_none()
                && string_from(raw, &["name"]).is_some();
            (
                if standalone {
                    "standalone_tool_output"
                } else {
                    "tool_result"
                },
                selected_map(
                    raw,
                    &[
                        "name",
                        "namespace",
                        "output",
                        "result",
                        "content",
                        "status",
                        "execution",
                        "tools",
                    ],
                ),
            )
        }
        "mcp_tool_call" | "dynamic_tool_call" | "collab_agent_tool_call" => (
            "tool_activity",
            selected_map(
                raw,
                &[
                    "server",
                    "tool",
                    "arguments",
                    "result",
                    "error",
                    "status",
                    "namespace",
                    "prompt",
                    "receiverThreadIds",
                ],
            ),
        ),
        "command_execution" | "command" | "shell_command" => (
            "command_execution",
            selected_map(
                raw,
                &[
                    "command",
                    "cwd",
                    "output",
                    "result",
                    "aggregatedOutput",
                    "exit_code",
                    "exitCode",
                    "status",
                ],
            ),
        ),
        "command_execution_output" | "command_execution_result" | "command_result" => (
            "command_execution_result",
            selected_map(raw, &["command", "output", "result", "exit_code", "status"]),
        ),
        "file_operation" | "file_change" | "file_edit" | "file_write" => (
            "file_operation",
            selected_map(
                raw,
                &[
                    "operation",
                    "path",
                    "diff",
                    "changes",
                    "content",
                    "result",
                    "status",
                ],
            ),
        ),
        "file_operation_result" | "file_change_result" | "file_edit_result" => (
            "file_operation_result",
            selected_map(raw, &["operation", "path", "result", "output", "status"]),
        ),
        "plan" | "plan_update" | "update_plan" => (
            "plan",
            first_value(raw, &["content", "text", "summary", "plan", "items"]),
        ),
        "user_constraint" | "constraint" => (
            "user_constraint",
            first_value(raw, &["content", "text", "constraint"]),
        ),
        "decision" => (
            "decision",
            first_value(raw, &["content", "text", "decision"]),
        ),
        "progress" => (
            "progress",
            first_value(raw, &["content", "text", "summary", "progress"]),
        ),
        "error" => (
            "error",
            first_value(raw, &["content", "text", "message", "error", "details"]),
        ),
        "blocker" => (
            "blocker",
            first_value(raw, &["content", "text", "message", "blocker", "details"]),
        ),
        "compaction" | "compaction_summary" | "compacted" | "compacted_summary" => (
            "compaction_summary",
            first_value(raw, &["message", "summary", "content", "text", "marker"]),
        ),
        "compaction_marker" | "compaction_boundary" | "context_compaction" => (
            "compaction_marker",
            first_value(raw, &["marker", "summary", "content", "text"]),
        ),
        "image" | "image_reference" | "input_image" | "output_image" => (
            "image_reference",
            selected_map(
                raw,
                &[
                    "image_url",
                    "url",
                    "file_id",
                    "detail",
                    "media_type",
                    "alt_text",
                ],
            ),
        ),
        _ => return None,
    };
    Some(VisibleItem {
        kind: visible_kind.to_owned(),
        content,
        item_id: string_from(raw, &["id", "item_id", "itemId"]),
        turn_id: turn.or_else(|| string_from(raw, &["turn_id", "turnId"])),
        call_id: string_from(raw, &["call_id", "callId"]),
        raw_type: Some(raw_type),
    })
}

fn message_role(record: &Map<String, Value>) -> Option<String> {
    let payload = record.get("payload")?.as_object()?;
    match token(record.get("type")).as_str() {
        "response_item" if token(payload.get("type")) == "message" => payload
            .get("role")?
            .as_str()
            .filter(|role| matches!(*role, "user" | "assistant"))
            .map(str::to_owned),
        "event_msg" => match token(payload.get("type")).as_str() {
            "user_message" | "usermessage" => Some("user".to_owned()),
            "assistant_message" | "assistantmessage" | "agent_message" | "agentmessage" => {
                Some("assistant".to_owned())
            }
            _ => None,
        },
        _ => None,
    }
}

fn record_turn_id(record: &Map<String, Value>) -> Option<String> {
    string_from(record, &["turn_id", "turnId"]).or_else(|| {
        record
            .get("payload")
            .and_then(Value::as_object)
            .and_then(|payload| string_from(payload, &["turn_id", "turnId"]))
    })
}

fn ordinal(record: &Map<String, Value>) -> Option<u64> {
    ["ordinal", "rollout_ordinal", "rolloutOrdinal", "sequence"]
        .iter()
        .find_map(|key| {
            record.get(*key).and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str()?.trim().parse().ok())
            })
        })
}

fn selected_map(raw: &Map<String, Value>, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                raw.get(*key)
                    .map(|value| ((*key).to_owned(), value.clone()))
            })
            .collect(),
    )
}

fn first_value(raw: &Map<String, Value>, keys: &[&str]) -> Value {
    keys.iter()
        .find_map(|key| raw.get(*key).cloned())
        .unwrap_or(Value::Null)
}

fn content_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(content_text)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(value) => ["text", "message", "content", "output", "result"]
            .iter()
            .find_map(|key| {
                value
                    .get(*key)
                    .map(content_text)
                    .filter(|text| !text.is_empty())
            })
            .unwrap_or_else(|| Value::Object(value.clone()).to_string()),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn string_from(raw: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        raw.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn token(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .enumerate()
        .flat_map(|(index, character)| {
            let separator = index > 0 && character.is_ascii_uppercase();
            std::iter::once(separator.then_some('_'))
                .flatten()
                .chain(std::iter::once(character.to_ascii_lowercase()))
        })
        .map(|character| {
            if matches!(character, '-' | ' ') {
                '_'
            } else {
                character
            }
        })
        .collect()
}

fn uuid_shape(value: &str) -> bool {
    value.len() == 36
        && value.chars().enumerate().all(|(index, character)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                character == '-'
            } else {
                character.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    const THREAD: &str = "01a00000-0000-7000-8000-000000000001";
    const TURN: &str = "01a00000-0000-7000-8000-000000000004";

    #[test]
    fn sqlite_and_rollout_are_read_only_and_failed_turns_are_excluded() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"turn_context","payload":{"turn_id":"old","model":"gpt-native"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"constraint"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"failed"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"do not replay"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"failed","error":{"code":"failed"}}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}),
        ];
        fs::write(
            &rollout,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let database = directory.path().join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)", []).unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'legacy', 'fallback')",
                params![THREAD, rollout.to_str().unwrap()],
            )
            .unwrap();
        drop(connection);
        let before_db = fs::read(&database).unwrap();
        let before_rollout = fs::read(&rollout).unwrap();
        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        assert_eq!(snapshot.source_model.as_deref(), Some("gpt-native"));
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| content_text(&item.content).contains("constraint"))
        );
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| content_text(&item.content).contains("do not replay"))
        );
        assert_eq!(fs::read(&database).unwrap(), before_db);
        assert_eq!(fs::read(&rollout).unwrap(), before_rollout);
    }
}
