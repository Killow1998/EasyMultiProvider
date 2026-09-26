//! Bounded, read-only reconstruction of Codex-visible rollout history.

use emp_history::{HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

mod records;
mod visible;

use records::*;
use visible::*;

const MAX_ROLLOUT_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ROLLOUT_SCAN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const REVERSE_SCAN_WINDOW_CAP: u64 = 64 * 1024 * 1024;
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

/// Byte offset where the newest replacement-history compaction record ends.
#[derive(Debug)]
struct ReverseBase {
    /// Records replayed instead of the full rollout prefix.
    replacement: Vec<Map<String, Value>>,
    /// File offset of the first record after the compaction record.
    suffix_start: u64,
    /// Turn owning the compaction record, carried from earlier records.
    turn: Option<String>,
}

/// Offset of the first record boundary at or after `start`.
///
/// Returns the offset of the first `\n` in the window plus one, or the
/// window end when no newline exists (no complete line in the window).
fn first_line_boundary(file: &mut File, start: u64, end: u64) -> Result<u64, HistoryError> {
    if start >= end {
        return Ok(end);
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(end - start));
    let mut offset = start;
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        if available.is_empty() {
            return Ok(end);
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            return Ok(offset + newline as u64 + 1);
        }
        offset += available.len() as u64;
        let take = available.len();
        reader.consume(take);
    }
}

/// Locate the newest replacement-history compaction by scanning backward.
///
/// Windows grow from the tail so large rollouts only parse a bounded suffix.
/// The returned suffix starts at the first record boundary after the
/// compaction record. `None` means reconstruction must replay the whole
/// rollout.
///
/// Selection mirrors Codex: only the newest compaction can bound replay. An
/// older compaction cannot replace the history or window state that a newer
/// one may have changed, so the newest compaction record decides on its own
/// whether the bounded replay applies. A paginated history accepts the newest
/// compaction when it carries a replacement history and window number; other
/// histories additionally require the newer resume metadata contract.
fn locate_latest_compaction(
    file: &mut File,
    captured_end: u64,
    history_mode: &str,
) -> Result<Option<ReverseBase>, HistoryError> {
    if captured_end == 0 {
        return Ok(None);
    }
    let mut window = captured_end.clamp(64 * 1024, 16 * 1024 * 1024);
    loop {
        let start = captured_end.saturating_sub(window);
        let line_start = first_line_boundary(file, start, captured_end)?;
        let length = (captured_end - line_start) as usize;
        file.seek(SeekFrom::Start(line_start))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(length as u64));
        let mut line = Vec::new();
        let mut candidate: Option<(ReverseBase, u64)> = None;
        let mut active_turn = None;
        let mut offset = line_start;
        loop {
            let Some(terminated) = read_bounded_line(&mut reader, &mut line)? else {
                break;
            };
            let record_start = offset;
            offset += line.len() as u64;
            match rollout_json_line(&line, terminated) {
                Ok(Some(record)) => {
                    if let Some(turn) = record_turn_id(&record) {
                        active_turn = Some(turn);
                    }
                    if token(record.get("type")).as_str() == "compacted" {
                        // Scanning forward, so each later compaction replaces
                        // the earlier one: the newest encountered record is
                        // the only candidate. Whether it is eligible decides
                        // the whole outcome; never fall back to an older one.
                        let replacement = replacement_history(&record)?;
                        let eligible = replacement.is_some()
                            && payload_window_number(&record).is_some()
                            && (history_mode == "paginated"
                                || payload_resume_metadata(&record).is_some())
                            // An opaque compaction item resolves against the
                            // pre-compaction visible history, so it is not
                            // self-contained; the full scan owns that case.
                            && !replacement.as_ref().is_some_and(|items| {
                                items.iter().any(|item| {
                                    item.get("type").and_then(Value::as_str)
                                        == Some("compaction")
                                        && item
                                            .get("encrypted_content")
                                            .and_then(Value::as_str)
                                            .is_some_and(|value| !value.is_empty())
                                })
                            });
                        candidate = eligible.then(|| {
                            (
                                ReverseBase {
                                    replacement: replacement.unwrap_or_default(),
                                    suffix_start: offset,
                                    turn: active_turn.clone(),
                                },
                                record_start,
                            )
                        });
                    }
                }
                Ok(None) => {}
                // Malformed complete lines are reported by the full scan.
                Err(HistoryError { .. }) if terminated => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        if let Some((base, _)) = candidate {
            return Ok(Some(base));
        }
        if start == 0 {
            // Whole file scanned without an eligible compaction base.
            return Ok(None);
        }
        if window >= REVERSE_SCAN_WINDOW_CAP {
            return Ok(None);
        }
        window = window.saturating_mul(2);
    }
}

/// Read the first complete record to verify the rollout's thread identity.
fn verify_session_thread(
    file: &mut File,
    captured_end: u64,
    expected: Option<&str>,
) -> Result<(), HistoryError> {
    if captured_end == 0 {
        return Err(HistoryError::new("session_meta_missing"));
    }
    let head = captured_end.min(MAX_ROLLOUT_LINE_BYTES as u64);
    file.seek(SeekFrom::Start(0))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(head));
    let mut line = Vec::new();
    while let Some(terminated) = read_bounded_line(&mut reader, &mut line)? {
        if let Some(record) = rollout_json_line(&line, terminated)?
            && matches!(
                token(record.get("type")).as_str(),
                "session_meta" | "sessionmeta"
            )
        {
            let id = session_meta_id(&record)
                .ok_or_else(|| HistoryError::new("session_meta_missing"))?;
            return match expected {
                Some(expected) if id != expected => Err(HistoryError::new("thread_mismatch")),
                _ => Ok(()),
            };
        }
    }
    Err(HistoryError::new("session_meta_missing"))
}

/// Read the lineage pointer from the rollout's first session meta record.
///
/// Returns `None` for rollouts without a `history_base`, which covers every
/// rollout written before Codex paginated persistent forks.
fn read_history_base(path: &Path) -> Result<Option<(String, u64)>, HistoryError> {
    let mut file = File::open(path).map_err(|_| HistoryError::new("source_missing"))?;
    let head = file
        .metadata()
        .map_err(|_| HistoryError::new("source_unavailable"))?
        .len()
        .min(MAX_ROLLOUT_LINE_BYTES as u64);
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(head));
    let mut line = Vec::new();
    while let Some(terminated) = read_bounded_line(&mut reader, &mut line)? {
        let Some(record) = rollout_json_line(&line, terminated)? else {
            continue;
        };
        if matches!(
            token(record.get("type")).as_str(),
            "session_meta" | "sessionmeta"
        ) {
            return Ok(session_meta_history_base(&record));
        }
    }
    Ok(None)
}

/// Forward scan of the records after a reverse-located compaction base.
fn scan_suffix(
    file: &mut File,
    suffix_start: u64,
    captured_end: u64,
    anchor: &HistoryAnchor,
) -> Result<(RolloutScan, bool), HistoryError> {
    file.seek(SeekFrom::Start(suffix_start))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let length = captured_end - suffix_start;
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(length));
    let mut line = Vec::new();
    let mut scan = RolloutScan::default();
    let mut active_turn = None;
    let mut index = 0usize;
    let anchor_turn = anchor.turn_id.clone();
    loop {
        let Some(terminated) = read_bounded_line(&mut reader, &mut line)? else {
            break;
        };
        let Some(record) = rollout_json_line(&line, terminated)? else {
            continue;
        };
        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());
        if anchor_turn
            .as_deref()
            .is_some_and(|turn| record_turn_id(&record).as_deref() == Some(turn))
        {
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
        }
        index += 1;
    }
    scan.total_records = index;
    scan.boundary = if anchor_turn.is_some() {
        match scan.anchor_boundary {
            Some(index) => index,
            // Anchor turn predates the compaction base: full scan required.
            None => return Ok((scan, false)),
        }
    } else if scan.explicit_turns.is_subset(&scan.terminal_turns) {
        scan.total_records
    } else {
        return Err(HistoryError::new("turn_not_found"));
    };
    Ok((scan, true))
}

/// Rebuild visible history from a reverse-located compaction base.
fn snapshot_from_base(
    file: &mut File,
    base: ReverseBase,
    mut scan: RolloutScan,
    (suffix_start, captured_end): (u64, u64),
    (anchor, location_mode, exact_compaction): (&HistoryAnchor, &str, bool),
    lineage_bound: Option<u64>,
) -> Result<HistorySnapshot, HistoryError> {
    let mut visible = replacement_entries(
        base.replacement,
        &Vec::<VisibleItem>::new(),
        base.turn.clone(),
    )?;
    if exact_compaction {
        // Inherited checkpoints capture the parent prefix through the
        // compaction record; newer parent records stay excluded.
        return Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().unwrap_or_default(),
            items: visible,
            source_model: None,
        });
    }
    if let Some(turn) = scan.last_turn.clone() {
        scan.successful.insert(turn, true);
    }
    file.seek(SeekFrom::Start(suffix_start))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let length = captured_end - suffix_start;
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(length));
    let mut line = Vec::new();
    let mut active_turn = None;
    let mut index = 0usize;
    loop {
        let Some(terminated) = read_bounded_line(&mut reader, &mut line)? else {
            break;
        };
        if index >= scan.boundary {
            break;
        }
        let Some(record) = rollout_json_line(&line, terminated)? else {
            continue;
        };
        if lineage_bound.is_some_and(|bound| ordinal(&record).is_some_and(|value| value >= bound)) {
            // The lineage cutoff owns everything from this ordinal on.
            break;
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
            continue;
        }
        if token(record.get("type")) == "event_msg"
            && message_role(&record)
                .is_some_and(|role| scan.response_roles.contains(&(turn.clone(), role)))
        {
            index += 1;
            continue;
        }
        if token(record.get("type")) == "event_msg" {
            let payload = record.get("payload").and_then(Value::as_object);
            if token(payload.and_then(|payload| payload.get("type"))).as_str()
                == "thread_rolled_back"
            {
                let num_turns = payload
                    .and_then(|payload| payload.get("num_turns"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                apply_thread_rollback(&mut visible, num_turns);
                index += 1;
                continue;
            }
        }
        let Some(raw) = visible_payload(&record) else {
            index += 1;
            continue;
        };
        if let Some(item) = normalize_visible_item(&raw, turn.clone()) {
            visible.push(item);
        }
        index += 1;
    }
    if location_mode == "paginated" && !scan.saw_ordinal {
        return Err(HistoryError::new("ordinal_missing"));
    }
    Ok(HistorySnapshot {
        thread_id: anchor.thread_id.clone().unwrap_or_default(),
        items: visible,
        // Suffix turns are the newest evidence; read_rollout falls back to
        // the threads table model for anchors without a turn id.
        source_model: scan.source_model(),
    })
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
        let prefix = if location.mode == "paginated" {
            self.lineage_prefix(&database, &location.path, &mut vec![thread.to_owned()])?
        } else {
            Vec::new()
        };
        let snapshot = self.read_rollout(anchor, &location, None, None)?;
        if prefix.is_empty() {
            return Ok(snapshot);
        }
        let mut items = prefix;
        items.extend(snapshot.items);
        Ok(HistorySnapshot {
            thread_id: snapshot.thread_id,
            items,
            source_model: snapshot.source_model,
        })
    }

    /// Inherited prefix for a paginated rollout: every ancestor segment
    /// recorded through `history_base` pointers, oldest first.
    ///
    /// Cycles and unbounded chains are rejected; a missing ancestor rollout
    /// surfaces as `source_missing` because the visible history would be
    /// silently incomplete otherwise.
    fn lineage_prefix(
        &self,
        database: &Path,
        path: &Path,
        visited: &mut Vec<String>,
    ) -> Result<Vec<VisibleItem>, HistoryError> {
        let mut prefix = Vec::new();
        let mut base = read_history_base(path)?;
        let mut chain = Vec::new();
        while let Some((parent_id, bound)) = base {
            if visited.contains(&parent_id) || chain.len() >= 32 {
                return Err(HistoryError::new("lineage_cycle"));
            }
            chain.push(parent_id.clone());
            visited.push(parent_id.clone());
            let parent_location = self.locate_parent(database, &parent_id)?;
            let parent_anchor = HistoryAnchor {
                thread_id: Some(parent_id.clone()),
                ..HistoryAnchor::default()
            };
            let mut snapshot =
                self.read_rollout(&parent_anchor, &parent_location, None, Some(bound))?;
            base = read_history_base(&parent_location.path)?;
            // Ancestors are resolved oldest-first, so each parent segment is
            // appended after the segments older than it.
            prefix.append(&mut snapshot.items);
        }
        Ok(prefix)
    }

    /// Resolve a lineage ancestor either through the threads database or by
    /// scanning the sessions tree for the rollout file name.
    fn locate_parent(&self, database: &Path, rollout_id: &str) -> Result<Location, HistoryError> {
        if let Ok(location) = locate(database, rollout_id) {
            return Ok(location);
        }
        let suffix = format!("{rollout_id}.jsonl");
        let mut found = None;
        let mut stack = vec![self.home.clone()];
        while let Some(directory) = stack.pop() {
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name.ends_with(&suffix))
                {
                    found = Some(path);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        found
            .map(|path| Location {
                path,
                mode: "paginated".to_owned(),
                source_model: None,
            })
            .ok_or_else(|| HistoryError::new("source_missing"))
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
        lineage_bound: Option<u64>,
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

        let expected_thread = anchor.thread_id.as_deref();
        // Reverse base applies only to paginated resumes: the newest
        // replacement-history compaction becomes the history base and replay
        // covers its suffix. Fork checkpoints (exact compaction) keep the full
        // scan because ambiguity detection must see every matching record.
        let reverse_base = if exact_compaction.is_none() && location.mode == "paginated" {
            locate_latest_compaction(&mut file, captured_end, &location.mode)?
        } else {
            None
        };
        let mut base_snapshot = None;
        if let Some(base) = reverse_base {
            verify_session_thread(&mut file, captured_end, expected_thread)?;
            let suffix_start = base.suffix_start;
            // Anchor inside the suffix and unchanged file: replay from base.
            // Anchor outside the suffix or malformed suffix: full scan.
            if let Ok((scan, true)) = scan_suffix(&mut file, suffix_start, captured_end, anchor)
                && file
                    .metadata()
                    .map_err(|_| HistoryError::new("source_unavailable"))?
                    .len()
                    >= captured_end
            {
                base_snapshot = Some(snapshot_from_base(
                    &mut file,
                    base,
                    scan,
                    (suffix_start, captured_end),
                    (anchor, &location.mode, exact_compaction.is_some()),
                    lineage_bound,
                )?);
            }
        }
        if let Some(snapshot) = base_snapshot {
            return Ok(HistorySnapshot {
                source_model: snapshot.source_model.or_else(|| {
                    anchor
                        .turn_id
                        .is_none()
                        .then(|| location.source_model.clone())
                        .flatten()
                }),
                ..snapshot
            });
        }

        // Reverse probing may leave the handle mid-file; restart at the top.
        file.seek(SeekFrom::Start(0))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
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
            if lineage_bound
                .is_some_and(|bound| ordinal(&record).is_some_and(|value| value >= bound))
            {
                // The lineage cutoff owns everything from this ordinal on.
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
            if token(record.get("type")) == "event_msg" {
                let payload = record.get("payload").and_then(Value::as_object);
                if token(payload.and_then(|payload| payload.get("type"))).as_str()
                    == "thread_rolled_back"
                {
                    let num_turns = payload
                        .and_then(|payload| payload.get("num_turns"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    apply_thread_rollback(&mut visible, num_turns);
                    index += 1;
                    return Ok(true);
                }
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
        let mut snapshot = self.read_rollout(&parent_anchor, &location, Some(compaction), None)?;
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
    const CHILD: &str = "01a00000-0000-7000-8000-000000000002";
    const PARENT: &str = "01a00000-0000-7000-8000-000000000003";
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

    #[test]
    fn paginated_resume_replays_from_newest_compaction_base() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"pre"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"pre-compaction only"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"pre"}}),
            json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
            "replacement_history":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"checkpoint base"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"checkpoint answer"}]}
            ]}}),
            json!({"ordinal":901,"type":"event_msg","payload":{"type":"task_started","turn_id":"post"}}),
            json!({"ordinal":902,"type":"turn_context","payload":{"turn_id":"post","model":"post-model"}}),
            json!({"ordinal":903,"type":"response_item","payload":{"type":"message","role":"user","content":"after checkpoint"}}),
            json!({"ordinal":904,"type":"event_msg","payload":{"type":"task_complete","turn_id":"post"}}),
            json!({"ordinal":905,"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}),
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

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("checkpoint base"), "{text}");
        assert!(text.contains("checkpoint answer"), "{text}");
        assert!(text.contains("after checkpoint"), "{text}");
        assert!(!text.contains("pre-compaction only"), "{text}");
    }

    #[test]
    fn paginated_base_falls_back_when_anchor_precedes_compaction() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}}),
            json!({"ordinal":0,"type":"event_msg","payload":{"type":"task_started","turn_id":"pre"}}),
            json!({"ordinal":1,"type":"response_item","payload":{"type":"message","role":"user","content":"pre turn"}}),
            json!({"ordinal":2,"type":"event_msg","payload":{"type":"task_complete","turn_id":"pre"}}),
            json!({"ordinal":3,"type":"event_msg","payload":{"type":"task_started","turn_id":"early"}}),
            json!({"ordinal":4,"type":"response_item","payload":{"type":"message","role":"user","content":"early turn"}}),
            json!({"ordinal":5,"type":"event_msg","payload":{"type":"task_complete","turn_id":"early"}}),
            json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
                "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"base"}]}]}}),
            json!({"ordinal":901,"type":"event_msg","payload":{"type":"task_started","turn_id":"post"}}),
            json!({"ordinal":902,"type":"event_msg","payload":{"type":"task_complete","turn_id":"post"}}),
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

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some("early".to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        // The anchor turn predates the compaction, so the full scan rebuilds
        // the prefix before the anchor turn: no base replay, no anchor items.
        assert!(text.contains("pre turn"), "{text}");
        assert!(!text.contains("early turn"), "{text}");
        assert!(!text.contains("base"), "{text}");
    }

    #[test]
    fn legacy_compaction_without_window_number_uses_full_scan() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"old"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":"legacy prefix"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"old"}}),
            json!({"type":"compacted","payload":{"message":"","replacement_history":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"compact base"}]}
            ]}}),
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
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        // A replacement-history compaction replaces the pre-compaction
        // prefix with its base in every scan mode.
        assert!(!text.contains("legacy prefix"), "{text}");
        // Legacy full scan replays everything through the compaction base.
        assert!(text.contains("compact base"), "{text}");
    }

    #[test]
    fn newer_compaction_without_window_number_blocks_older_base() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"paginated"}}),
            json!({"ordinal":0,"type":"event_msg","payload":{"type":"task_started","turn_id":"pre"}}),
            json!({"ordinal":1,"type":"response_item","payload":{"type":"message","role":"user","content":"pre-compaction prefix"}}),
            json!({"ordinal":2,"type":"event_msg","payload":{"type":"task_complete","turn_id":"pre"}}),
            // Older, fully eligible checkpoint.
            json!({"ordinal":900,"type":"compacted","payload":{"message":"","window_number":1,
                "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"older base"}]}]}}),
            json!({"ordinal":901,"type":"event_msg","payload":{"type":"task_started","turn_id":"mid"}}),
            json!({"ordinal":902,"type":"response_item","payload":{"type":"message","role":"user","content":"between checkpoints"}}),
            json!({"ordinal":903,"type":"event_msg","payload":{"type":"task_complete","turn_id":"mid"}}),
            // Newest checkpoint is ineligible: no window number. Codex never
            // falls back to the older base, so the full scan owns the rebuild.
            json!({"ordinal":910,"type":"compacted","payload":{"message":"","replacement_history":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"newer base"}]}]}}),
            json!({"ordinal":911,"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}),
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

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        // The newest compaction carries a replacement history, so the prefix
        // it replaced must never reappear; the ineligible newest checkpoint
        // forces full replay rather than the older base.
        assert!(!text.contains("pre-compaction prefix"), "{text}");
        assert!(!text.contains("older base"), "{text}");
        // The newest compaction's replacement history supersedes everything
        // recorded before it, including the older base and the interim turn.
        assert!(text.contains("newer base"), "{text}");
        assert!(!text.contains("between checkpoints"), "{text}");
    }

    #[test]
    fn legacy_compaction_without_resume_metadata_blocks_base() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            json!({"type":"compacted","payload":{"message":"","window_number":1,
                "replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"legacy base"}]}]}}),
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
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

        // The reverse path never engages for legacy rollouts; assert the gate
        // helper directly so the resume-metadata requirement stays pinned.
        let file = File::open(&rollout).unwrap();
        let captured_end = file.metadata().unwrap().len();
        let mut file = file;
        let base = locate_latest_compaction(&mut file, captured_end, "legacy").unwrap();
        assert!(base.is_none());
        let base = locate_latest_compaction(&mut file, captured_end, "paginated").unwrap();
        assert!(base.is_some());
    }

    #[test]
    fn thread_rollback_removes_rolled_back_turns() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            // Turn A: real user message and assistant reply.
            json!({"type":"event_msg","payload":{"type":"user_message","message":"user A"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"assistant A"}}),
            // Turn B: real user message, assistant reply, then a rollback that
            // removes it. The external model must never see turn B.
            json!({"type":"event_msg","payload":{"type":"user_message","message":"user B"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"assistant B"}}),
            json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
            // Resumed work continues from the post-rollback history.
            json!({"type":"event_msg","payload":{"type":"user_message","message":"user C"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"assistant C"}}),
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
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("user A"), "{text}");
        assert!(text.contains("assistant A"), "{text}");
        assert!(!text.contains("user B"), "{text}");
        assert!(!text.contains("assistant B"), "{text}");
        assert!(text.contains("user C"), "{text}");
        assert!(text.contains("assistant C"), "{text}");
    }

    #[test]
    fn rollback_keeps_contextual_user_messages() {
        let directory = tempdir().unwrap();
        let rollout = directory.path().join("rollout.jsonl");
        let records = [
            json!({"type":"session_meta","payload":{"id":THREAD,"history_mode":"legacy"}}),
            // Contextual user messages are not user turns.
            json!({"type":"event_msg","payload":{"type":"user_message","message":"<user_instructions>\nAGENTS\n</user_instructions>"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"real turn one"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"real turn two"}}),
            json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":1}}),
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
        write_state_database(directory.path(), &rollout, THREAD, "legacy");

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(THREAD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("user_instructions"), "{text}");
        assert!(text.contains("real turn one"), "{text}");
        assert!(!text.contains("real turn two"), "{text}");
    }

    #[test]
    fn paginated_lineage_prepends_ancestor_prefix() {
        let directory = tempdir().unwrap();
        let parent = directory
            .path()
            .join(format!("rollout-2026-09-26T00-00-00-{PARENT}.jsonl"));
        let parent_records = [
            json!({"type":"session_meta","payload":{"id":PARENT,"history_mode":"paginated"}}),
            json!({"ordinal":1,"type":"event_msg","payload":{"type":"user_message","message":"parent turn"}}),
            json!({"ordinal":2,"type":"event_msg","payload":{"type":"agent_message","message":"parent answer"}}),
            // Everything from ordinal 3 on belongs to the child, never the
            // ancestor prefix.
            json!({"ordinal":3,"type":"event_msg","payload":{"type":"user_message","message":"parent excluded"}}),
        ];
        fs::write(
            &parent,
            parent_records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let child = directory
            .path()
            .join(format!("rollout-2026-09-26T00-00-01-{CHILD}.jsonl"));
        let child_records = [
            json!({"type":"session_meta","payload":{"id":CHILD,"history_mode":"paginated",
                "history_base":{"thread_id":PARENT,"end_ordinal_exclusive":3,"end_byte_offset":0}}}),
            json!({"ordinal":3,"type":"event_msg","payload":{"type":"user_message","message":"child turn"}}),
            json!({"ordinal":4,"type":"event_msg","payload":{"type":"agent_message","message":"child answer"}}),
            json!({"ordinal":5,"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}),
        ];
        fs::write(
            &child,
            child_records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();

        let database = directory.path().join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'paginated', 'gpt-native')",
                params![CHILD, child.to_str().unwrap()],
            )
            .unwrap();

        let snapshot = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(CHILD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap();
        let text = snapshot
            .items
            .iter()
            .map(|item| content_text(&item.content))
            .collect::<Vec<_>>()
            .join("\n");
        // The full visible history is the ancestor segment followed by the
        // child's own records; the post-cutoff parent record stays excluded.
        assert!(text.contains("parent turn"), "{text}");
        assert!(text.contains("parent answer"), "{text}");
        assert!(!text.contains("parent excluded"), "{text}");
        assert!(text.contains("child turn"), "{text}");
        assert!(text.contains("child answer"), "{text}");
    }

    #[test]
    fn lineage_cycle_is_rejected() {
        let directory = tempdir().unwrap();
        let child = directory
            .path()
            .join(format!("rollout-2026-09-26T00-00-02-{CHILD}.jsonl"));
        let records = [
            json!({"type":"session_meta","payload":{"id":CHILD,"history_mode":"paginated",
                "history_base":{"thread_id":CHILD,"end_ordinal_exclusive":2,"end_byte_offset":0}}}),
            json!({"ordinal":1,"type":"event_msg","payload":{"type":"user_message","message":"cyclic"}}),
            json!({"ordinal":2,"type":"event_msg","payload":{"type":"task_started","turn_id":TURN}}),
        ];
        fs::write(
            &child,
            records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let database = directory.path().join("state_5.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads VALUES (?1, ?2, 'paginated', 'gpt-native')",
                params![CHILD, child.to_str().unwrap()],
            )
            .unwrap();

        let error = CodexHomeHistoryReader::new(directory.path())
            .read_visible_history(&HistoryAnchor {
                thread_id: Some(CHILD.to_owned()),
                turn_id: Some(TURN.to_owned()),
                ..HistoryAnchor::default()
            })
            .unwrap_err();
        assert_eq!(error.reason(), "lineage_cycle");
    }
}
