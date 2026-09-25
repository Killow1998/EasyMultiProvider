//! Bounded, read-only reconstruction of Codex-visible rollout history.

use emp_history::{HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

mod records;
mod visible;

use records::*;
use visible::*;

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
