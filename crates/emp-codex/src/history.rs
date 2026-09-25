//! Bounded, read-only reconstruction of Codex-visible rollout history.

use emp_history::{HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

mod records;
mod visible;

use records::*;
use visible::*;

const MAX_ROLLOUT_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ROLLOUT_SCAN_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug)]
struct Location {
    path: PathBuf,
    mode: String,
    source_model: Option<String>,
}

#[derive(Debug, Default)]
struct RolloutScan {
    thread_ids: BTreeSet<String>,
    successful: BTreeMap<String, bool>,
    successful_models: Vec<(String, Option<String>)>,
    response_roles: BTreeSet<(Option<String>, String)>,
    saw_ordinal: bool,
    explicit_turns: BTreeSet<String>,
    terminal_turns: BTreeSet<String>,
    boundary: usize,
    total_records: usize,
    exact_matches: usize,
    exact_boundary: usize,
    anchor_boundary: Option<usize>,
    last_turn: Option<String>,
    history_closed: bool,
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
        let mut file = File::open(&path).map_err(|_| HistoryError::new("source_unavailable"))?;
        let opened = file
            .metadata()
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        if !opened.is_file() {
            return Err(HistoryError::new("source_missing"));
        }
        let captured_end = opened.len();
        if captured_end > MAX_ROLLOUT_SCAN_BYTES {
            return Err(HistoryError::new("source_too_large"));
        }

        let mut scan = scan_rollout(&mut file, captured_end, anchor, exact_compaction)?;
        if file
            .metadata()
            .map_err(|_| HistoryError::new("source_unavailable"))?
            .len()
            < captured_end
        {
            return Err(HistoryError::new("source_changed"));
        }

        if exact_compaction.is_some()
            && let Some(turn) = scan.last_turn.clone()
        {
            scan.successful.insert(turn, true);
        }

        file.seek(SeekFrom::Start(0))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        let mut visible = Vec::<VisibleItem>::new();
        let mut active_turn = None;
        let mut index = 0usize;
        stream_rollout(&mut file, captured_end, |record| {
            if index >= scan.boundary {
                return Ok(false);
            }
            let explicit_turn = record_turn_id(&record);
            if explicit_turn.is_some() {
                active_turn = explicit_turn.clone();
            }
            let turn = explicit_turn.clone().or_else(|| active_turn.clone());
            if turn
                .as_ref()
                .is_some_and(|turn| !scan.successful.get(turn).copied().unwrap_or(false))
            {
                index += 1;
                return Ok(true);
            }
            if token(record.get("type")) == "event_msg"
                && message_role(&record)
                    .is_some_and(|role| scan.response_roles.contains(&(turn.clone(), role)))
            {
                index += 1;
                return Ok(true);
            }
            if let Some(replacement) = replacement_history(&record)? {
                visible = replacement_entries(replacement, &visible, turn.clone())?;
                index += 1;
                return Ok(true);
            }
            let Some(raw) = visible_payload(&record) else {
                index += 1;
                return Ok(true);
            };
            if let Some(item) = normalize_visible_item(&raw, turn.clone()) {
                visible.push(item);
            }
            index += 1;
            Ok(true)
        })?;
        if location.mode == "paginated" && !scan.saw_ordinal {
            return Err(HistoryError::new("ordinal_missing"));
        }
        Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().unwrap_or_default(),
            items: visible,
            source_model: scan.source_model().or_else(|| {
                anchor
                    .turn_id
                    .is_none()
                    .then(|| location.source_model.clone())
                    .flatten()
            }),
        })
    }
}

impl RolloutScan {
    fn source_model(&self) -> Option<String> {
        self.successful_models
            .iter()
            .rev()
            .find_map(|(turn, model)| {
                self.successful
                    .get(turn)
                    .copied()
                    .unwrap_or(false)
                    .then(|| model.clone())
                    .flatten()
            })
    }
}

fn scan_rollout(
    file: &mut File,
    captured_end: u64,
    anchor: &HistoryAnchor,
    exact_compaction: Option<&Map<String, Value>>,
) -> Result<RolloutScan, HistoryError> {
    let exact_encoded = exact_compaction
        .and_then(|compaction| compaction.get("encrypted_content"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expected_thread = anchor.thread_id.as_deref();
    let anchor_turn = anchor.turn_id.as_deref();
    let mut scan = RolloutScan::default();
    let mut active_turn = None;
    let mut index = 0usize;

    stream_rollout(file, captured_end, |record| {
        let record_type = token(record.get("type"));
        if matches!(record_type.as_str(), "session_meta" | "sessionmeta")
            && let Some(id) = session_meta_id(&record)
        {
            scan.thread_ids.insert(id);
        }

        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());

        if !scan.history_closed {
            if anchor_turn.is_some_and(|turn| record_turn_id(&record).as_deref() == Some(turn)) {
                scan.anchor_boundary = Some(index);
                scan.boundary = index;
                scan.history_closed = true;
            } else {
                scan.last_turn = turn.clone();
                if let Some(turn) = &turn {
                    if let Some(explicit) = explicit_turn.as_ref() {
                        scan.explicit_turns.insert(explicit.clone());
                    }
                    let payload_type = record
                        .get("payload")
                        .and_then(Value::as_object)
                        .map(|payload| token(payload.get("type")))
                        .unwrap_or_default();
                    if token(record.get("type")) == "event_msg" && payload_type == "task_complete" {
                        scan.terminal_turns.insert(turn.clone());
                        scan.successful.insert(
                            turn.clone(),
                            record
                                .get("payload")
                                .and_then(Value::as_object)
                                .and_then(|payload| payload.get("error"))
                                .is_none_or(Value::is_null),
                        );
                    }
                }
                if token(record.get("type")) == "response_item"
                    && let Some(role) = message_role(&record)
                {
                    scan.response_roles.insert((turn.clone(), role));
                }
                if token(record.get("type")) == "turn_context"
                    && let Some(turn) = record_turn_id(&record)
                {
                    let payload = record
                        .get("payload")
                        .and_then(Value::as_object)
                        .unwrap_or(&record);
                    scan.successful_models.push((
                        turn,
                        string_from(payload, &["model", "model_id", "modelId", "selected_model"]),
                    ));
                }
                if ordinal(&record).is_some() {
                    scan.saw_ordinal = true;
                }
                if exact_compaction.is_some()
                    && replacement_contains_encoded(&record, exact_encoded.unwrap_or_default())
                {
                    scan.exact_matches += 1;
                    if scan.exact_matches == 1 {
                        scan.exact_boundary = index + 1;
                        scan.boundary = index + 1;
                        scan.history_closed = true;
                    }
                }
            }
        } else if exact_compaction.is_some()
            && replacement_contains_encoded(&record, exact_encoded.unwrap_or_default())
        {
            scan.exact_matches += 1;
        }

        index += 1;
        Ok(true)
    })?;

    scan.total_records = index;
    validate_scan_thread(&scan, expected_thread)?;
    scan.boundary = if exact_compaction.is_some() {
        match scan.exact_matches {
            1 => scan.exact_boundary,
            0 => return Err(HistoryError::new("compaction_identity_missing")),
            _ => return Err(HistoryError::new("compaction_identity_ambiguous")),
        }
    } else if anchor_turn.is_none() {
        scan.total_records
    } else if let Some(index) = scan.anchor_boundary {
        index
    } else if scan.explicit_turns.is_subset(&scan.terminal_turns) {
        scan.total_records
    } else {
        return Err(HistoryError::new("turn_not_found"));
    };
    Ok(scan)
}

fn validate_scan_thread(scan: &RolloutScan, expected: Option<&str>) -> Result<(), HistoryError> {
    if scan.thread_ids.is_empty() {
        return Err(HistoryError::new("session_meta_missing"));
    }
    if scan
        .thread_ids
        .iter()
        .any(|id| Some(id.as_str()) != expected)
    {
        return Err(HistoryError::new("thread_mismatch"));
    }
    if scan.thread_ids.len() != 1 {
        return Err(HistoryError::new("thread_identity_conflict"));
    }
    Ok(())
}

fn stream_rollout(
    file: &mut File,
    captured_end: u64,
    mut visit: impl FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
) -> Result<(), HistoryError> {
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(captured_end));
    let mut line = Vec::new();
    while let Some(terminated) = read_bounded_line(&mut reader, &mut line)? {
        if let Some(record) = rollout_json_line(&line, terminated)?
            && !visit(record)?
        {
            return Ok(());
        }
    }
    Ok(())
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> Result<Option<bool>, HistoryError> {
    line.clear();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(false));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            let take = newline + 1;
            if line.len().saturating_add(take) > MAX_ROLLOUT_LINE_BYTES {
                return Err(HistoryError::new("history_record_too_large"));
            }
            line.extend_from_slice(&available[..take]);
            reader.consume(take);
            return Ok(Some(true));
        }
        if line.len().saturating_add(available.len()) > MAX_ROLLOUT_LINE_BYTES {
            return Err(HistoryError::new("history_record_too_large"));
        }
        line.extend_from_slice(available);
        let take = available.len();
        reader.consume(take);
    }
}

fn rollout_json_line(
    line: &[u8],
    terminated: bool,
) -> Result<Option<Map<String, Value>>, HistoryError> {
    let raw = if terminated {
        &line[..line.len().saturating_sub(1)]
    } else {
        line
    };
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    if raw.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(value)) => Ok(Some(value)),
        Ok(_) => Err(HistoryError::new("invalid_json")),
        Err(error) if !terminated && error.is_eof() => Ok(None),
        Err(_) => Err(HistoryError::new("invalid_json")),
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

    fn write_state_database(directory: &Path, rollout: &Path, thread: &str, mode: &str) {
        let database = directory.join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, ?3, 'gpt-native')",
                params![thread, rollout.to_str().unwrap(), mode],
            )
            .unwrap();
    }

    #[test]
    fn rollout_over_legacy_limit_streams_without_source_too_large() {
        use std::io::Write as _;

        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let mut file = File::create(&rollout).unwrap();
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"turn_context","payload":{"turn_id":"old","model":"gpt-native"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"kept before legacy limit"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}),
        ];
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
        let filler = " ".repeat(64 * 1024 - 1);
        for _ in 0..2049 {
            writeln!(file, "{filler}").unwrap();
        }
        write!(file, "{{\"type\":\"response_item\"").unwrap();
        drop(file);
        assert!(fs::metadata(&rollout).unwrap().len() > 128 * 1024 * 1024);
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

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
                .any(|item| content_text(&item.content).contains("kept before legacy limit"))
        );
    }

    #[test]
    fn oversized_rollout_line_is_bounded() {
        use std::io::Write as _;

        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let mut file = File::create(&rollout).unwrap();
        writeln!(
            file,
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{THREAD}\",\"history_mode\":\"legacy\"}}}}"
        )
        .unwrap();
        let line = vec![b' '; MAX_ROLLOUT_LINE_BYTES + 1];
        file.write_all(&line).unwrap();
        writeln!(file).unwrap();
        drop(file);
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

        let error = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap_err();
        assert_eq!(error.reason(), "history_record_too_large");
    }

    #[test]
    fn partial_trailing_json_line_is_ignored() {
        use std::io::Write as _;

        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let mut file = File::create(&rollout).unwrap();
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"complete record"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
        ];
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
        write!(
            file,
            "{{\"type\":\"response_item\",\"payload\":{{\"content\":\"partial"
        )
        .unwrap();
        drop(file);
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        assert!(
            snapshot
                .items
                .iter()
                .any(|item| content_text(&item.content).contains("complete record"))
        );
        assert!(
            !snapshot
                .items
                .iter()
                .any(|item| content_text(&item.content).contains("partial"))
        );
    }

    #[test]
    fn duplicate_exact_compaction_is_ambiguous() {
        const CHILD: &str = "01a00000-0000-7000-8000-000000000002";
        const PARENT: &str = "01a00000-0000-7000-8000-000000000003";
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("parent.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":PARENT,"history_mode":"legacy"}}),
            json!({"type":"compacted","payload":{"replacement_history":[{"type":"compaction","encrypted_content":"same"}]}}),
            json!({"type":"compacted","payload":{"replacement_history":[{"type":"compaction","encrypted_content":"same"}]}}),
        ];
        fs::write(
            &rollout,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        write_state_database(directory.path(), &rollout, PARENT, "legacy");

        let compaction = json!({"encrypted_content":"same"});
        let error = CodexHomeHistoryReader::new(directory.path())
            .read_compaction_history(
                &HistoryAnchor {
                    thread_id: Some(CHILD.to_owned()),
                    forked_from_thread_id: Some(PARENT.to_owned()),
                    ..HistoryAnchor::default()
                },
                compaction.as_object().unwrap(),
            )
            .unwrap_err();
        assert_eq!(error.reason(), "compaction_identity_ambiguous");
    }

    #[test]
    fn paginated_rollout_without_ordinal_is_rejected() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
        ];
        fs::write(
            &rollout,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        write_state_database(directory.path(), &rollout, THREAD, "paginated");

        let error = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap_err();
        assert_eq!(error.reason(), "ordinal_missing");
    }
}
