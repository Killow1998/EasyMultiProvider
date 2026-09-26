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

/// Replay wiring for one rollout read: lineage bounds, fork checkpoint, and
/// the seed history.
struct ReplayContext<'a> {
    exact_compaction: Option<&'a Map<String, Value>>,
    lineage_bound: Option<u64>,
    lineage_byte_offset: u64,
    seed: Vec<VisibleItem>,
}

impl<'a> ReplayContext<'a> {
    fn resume() -> Self {
        Self {
            exact_compaction: None,
            lineage_bound: None,
            lineage_byte_offset: 0,
            seed: Vec::new(),
        }
    }
}

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
    /// Newest ordinal observed on a paginated rollout, for monotonicity.
    last_ordinal: Option<u64>,
    /// An ordinal regressed below the newest one: malformed pagination.
    ordinal_regressed: bool,
    /// A record inside the replay window lacks the ordinal Codex promises.
    ordinal_missing: bool,
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
    /// Turn completion states observed before the compaction record; the
    /// suffix-local scan cannot see them, but the failed-turn filter needs
    /// them to match full-scan replay.
    successful: BTreeMap<String, bool>,
    response_roles: BTreeSet<(Option<String>, String)>,
}

/// One rollout segment of a paginated lineage, replayed oldest first.
struct LineageSegment {
    /// Rollout identifier owning the segment.
    thread: String,
    /// Resolved rollout file and history mode.
    location: Location,
    /// Records at or above this ordinal belong to the next-younger segment.
    bound: Option<u64>,
    /// Frozen physical prefix (`history_base.end_byte_offset`); 0 disables
    /// the byte cutoff, keeping the ordinal bound as the only boundary.
    byte_offset: u64,
}

/// Offset of the first record boundary at or after `start`.
///
/// When `start` already sits exactly on a record start (offset 0, or the
/// byte before it is `\n`), the record beginning there is inside the window
/// and must stay visible; otherwise the boundary is the first `\n` in the
/// window plus one. Window end is returned when no complete line exists.
fn first_line_boundary(file: &mut File, start: u64, end: u64) -> Result<u64, HistoryError> {
    if start >= end {
        return Ok(end);
    }
    if start == 0 {
        return Ok(0);
    }
    file.seek(SeekFrom::Start(start - 1))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut preceding = [0u8; 1];
    file.read_exact(&mut preceding)
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    if preceding[0] == b'\n' {
        return Ok(start);
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
        let mut successful: BTreeMap<String, bool> = BTreeMap::new();
        let mut response_roles: BTreeSet<(Option<String>, String)> = BTreeSet::new();
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
                                    successful: successful.clone(),
                                    response_roles: response_roles.clone(),
                                },
                                record_start,
                            )
                        });
                    }
                    let turn = record_turn_id(&record).or_else(|| active_turn.clone());
                    if let Some(turn) = &turn {
                        let payload = record.get("payload").and_then(Value::as_object);
                        if token(record.get("type")) == "event_msg"
                            && payload.map(|payload| token(payload.get("type"))).as_deref()
                                == Some("task_complete")
                        {
                            successful.insert(
                                turn.clone(),
                                payload
                                    .and_then(|payload| payload.get("error"))
                                    .is_none_or(Value::is_null),
                            );
                        }
                    }
                    if token(record.get("type")) == "response_item"
                        && let Some(role) = message_role(&record)
                    {
                        response_roles.insert((turn, role));
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
fn read_history_base(path: &Path) -> Result<Option<(String, u64, u64)>, HistoryError> {
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

/// Track paginated ordinal monotonicity while scanning.
///
/// A missing ordinal on an otherwise parsed record, or an ordinal that
/// regresses below the newest one, marks malformed pagination (checked after
/// the scan). Forward gaps stay legal: unknown or future records may skip.
fn track_ordinal(scan: &mut RolloutScan, record: &Map<String, Value>) {
    match ordinal(record) {
        Some(value) => {
            scan.saw_ordinal = true;
            if scan.last_ordinal.is_some_and(|last| value < last) {
                scan.ordinal_regressed = true;
            }
            scan.last_ordinal = Some(scan.last_ordinal.map_or(value, |last| last.max(value)));
        }
        None => scan.ordinal_missing = true,
    }
}

/// Forward scan of the records after a reverse-located compaction base.
/// One replay pass over a rollout byte range.
///
/// The reverse-base path starts after a compaction record and seeds the replay
/// with that record's replacement history plus the pre-base turn state the
/// suffix scan cannot see. Every other path replays from the file head with an
/// empty seed.
struct ReplayFrame {
    /// First replayed byte offset.
    start: u64,
    /// Exclusive end offset (`captured_end` or a lineage byte cap).
    end: u64,
    /// Visible items rebuilt before the first replayed record.
    seed_visible: Vec<VisibleItem>,
    /// Turn owning the seed, carried into records without a turn id.
    seed_turn: Option<String>,
    /// Turn completion states observed before `start`.
    seed_successful: BTreeMap<String, bool>,
    /// Response roles observed before `start`.
    seed_roles: BTreeSet<(Option<String>, String)>,
}

impl ReplayFrame {
    fn head(end: u64) -> Self {
        Self {
            start: 0,
            end,
            seed_visible: Vec::new(),
            seed_turn: None,
            seed_successful: BTreeMap::new(),
            seed_roles: BTreeSet::new(),
        }
    }
}
/// A scan pass outcome: either the suffix fully determines the boundary or
/// the caller must fall back to the full scan.
enum SuffixScan {
    Usable(Box<RolloutScan>),
    FallBack,
}

/// Forward scan of the records after a reverse-located compaction base.
fn scan_suffix(
    file: &mut File,
    frame: &ReplayFrame,
    anchor: &HistoryAnchor,
) -> Result<SuffixScan, HistoryError> {
    file.seek(SeekFrom::Start(frame.start))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut scan = RolloutScan::default();
    let mut active_turn = frame.seed_turn.clone();
    let mut index = 0usize;
    walk_records(file, frame.end, |record| {
        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());
        // Ordinal integrity covers every parsed record, including the
        // anchor hit itself; skipping it would let a suffix that contains
        // only the anchor pass off a missing-ordinal defect.
        track_ordinal(&mut scan, &record);
        if anchor
            .turn_id
            .as_deref()
            .is_some_and(|turn| record_turn_id(&record).as_deref() == Some(turn))
        {
            scan.anchor_boundary = Some(index);
            scan.boundary = index;
            scan.history_closed = true;
        } else {
            scan.last_turn = turn.clone();
            observe_scan_record(&mut scan, &record, explicit_turn, turn);
        }
        index += 1;
        Ok(true)
    })?;
    scan.total_records = index;
    let usable = if anchor.turn_id.is_some() {
        // Anchor turn predates the compaction base: full scan required.
        scan.anchor_boundary.is_some()
    } else {
        scan.explicit_turns.is_subset(&scan.terminal_turns)
    };
    if !usable {
        return Ok(SuffixScan::FallBack);
    }
    scan.boundary = scan.anchor_boundary.unwrap_or(scan.total_records);
    if scan.ordinal_regressed {
        // Malformed pagination: treat like an unusable suffix and fall back
        // to the full scan, which reports the same defect terminally.
        return Ok(SuffixScan::FallBack);
    }
    Ok(SuffixScan::Usable(Box::new(scan)))
}

/// Shared per-record scan bookkeeping: turns, completion states, response
/// roles, model contexts. The boundary record itself is handled by the caller.
fn observe_scan_record(
    scan: &mut RolloutScan,
    record: &Map<String, Value>,
    explicit_turn: Option<String>,
    turn: Option<String>,
) {
    if let Some(turn) = &turn {
        if let Some(explicit) = explicit_turn {
            scan.explicit_turns.insert(explicit);
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
        && let Some(role) = message_role(record)
    {
        scan.response_roles.insert((turn, role));
    }
    if token(record.get("type")) == "turn_context"
        && let Some(turn) = record_turn_id(record)
    {
        let payload = record
            .get("payload")
            .and_then(Value::as_object)
            .unwrap_or(record);
        scan.successful_models.push((
            turn,
            string_from(payload, &["model", "model_id", "modelId", "selected_model"]),
        ));
    }
}

/// Replay the frame's records into `visible`, applying rollback markers, the
/// failed-turn filter, the response-role echo filter, and replacement-history
/// compaction records. Returns `None` when the boundary was hit and replay
/// must stop.
fn replay_records(
    file: &mut File,
    frame: &ReplayFrame,
    scan: &RolloutScan,
    anchor: &HistoryAnchor,
    lineage_bound: Option<u64>,
) -> Result<Vec<VisibleItem>, HistoryError> {
    let mut visible = frame.seed_visible.clone();
    // The anchored turn is in flight by definition; never let the failed-turn
    // filter swallow its records. Other turns keep the completion state that
    // both paths observed.
    let mut successful = frame.seed_successful.clone();
    for (turn, ok) in &scan.successful {
        successful.insert(turn.clone(), *ok);
    }
    if let Some(turn) = anchor.turn_id.clone() {
        successful.insert(turn, true);
    }
    let mut roles = frame.seed_roles.clone();
    roles.extend(scan.response_roles.iter().cloned());
    file.seek(SeekFrom::Start(frame.start))
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    let mut active_turn = frame.seed_turn.clone();
    let mut index = 0usize;
    walk_records(file, frame.end, |record| {
        if index >= scan.boundary {
            return Ok(false);
        }
        if lineage_bound.is_some_and(|bound| ordinal(&record).is_some_and(|value| value >= bound)) {
            // The lineage cutoff owns everything from this ordinal on.
            return Ok(false);
        }
        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());
        // Rollback control markers carry no turn id and must survive the
        // failed-turn filter: an interrupted turn followed by a rollback
        // would otherwise swallow the marker and leak the stale suffix.
        if token(record.get("type")) == "event_msg"
            && token(
                record
                    .get("payload")
                    .and_then(Value::as_object)
                    .and_then(|payload| payload.get("type")),
            )
            .as_str()
                == "thread_rolled_back"
        {
            let num_turns = record
                .get("payload")
                .and_then(Value::as_object)
                .and_then(|payload| payload.get("num_turns"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            apply_thread_rollback(&mut visible, num_turns);
            // The rolled-back turn is destroyed; records without an explicit
            // turn id after the marker begin fresh turns instead of inheriting
            // the interrupted turn's failure state.
            active_turn = None;
            index += 1;
            return Ok(true);
        }
        if turn
            .as_ref()
            .is_some_and(|turn| !successful.get(turn).copied().unwrap_or(false))
        {
            index += 1;
            return Ok(true);
        }
        if token(record.get("type")) == "event_msg"
            && message_role(&record).is_some_and(|role| roles.contains(&(turn.clone(), role)))
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
    Ok(visible)
}

/// Rebuild visible history from a reverse-located compaction base.
#[allow(clippy::too_many_arguments)]
fn snapshot_from_base(
    file: &mut File,
    base: ReverseBase,
    scan: RolloutScan,
    frame: ReplayFrame,
    anchor: &HistoryAnchor,
    exact_compaction: bool,
    lineage_bound: Option<u64>,
) -> Result<HistorySnapshot, HistoryError> {
    let mut frame = frame;
    frame.seed_visible = replacement_entries(
        base.replacement,
        &Vec::<VisibleItem>::new(),
        base.turn.clone(),
    )?;
    if exact_compaction {
        // Inherited checkpoints capture the parent prefix through the
        // compaction record; newer parent records stay excluded.
        return Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().unwrap_or_default(),
            items: frame.seed_visible,
            source_model: None,
        });
    }
    let items = replay_records(file, &frame, &scan, anchor, lineage_bound)?;
    Ok(HistorySnapshot {
        thread_id: anchor.thread_id.clone().unwrap_or_default(),
        items,
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
        if location.mode != "paginated" {
            return self.read_rollout(anchor, &location, ReplayContext::resume());
        }
        // The lineage is one logical rollout: ancestors replay oldest first
        // into the same visible vector, and the child replays on top of that
        // state. A self-contained child compaction then replaces the whole
        // vector (ancestors drop out), while an opaque child compaction
        // resolves against the merged pre-compaction history exactly as the
        // full scan would.
        let segments = self.lineage_segments(&database, &location.path, thread)?;
        let mut visible = Vec::<VisibleItem>::new();
        let mut source_model = None;
        for (position, segment) in segments.iter().enumerate() {
            let child = position + 1 == segments.len();
            let anchor = if child {
                anchor.clone()
            } else {
                HistoryAnchor {
                    thread_id: Some(segment.thread.clone()),
                    ..HistoryAnchor::default()
                }
            };
            let snapshot = self.read_rollout(
                &anchor,
                &segment.location,
                ReplayContext {
                    lineage_bound: segment.bound,
                    lineage_byte_offset: segment.byte_offset,
                    seed: std::mem::take(&mut visible),
                    ..ReplayContext::resume()
                },
            )?;
            visible = snapshot.items;
            if snapshot.source_model.is_some() {
                source_model = snapshot.source_model;
            }
        }
        Ok(HistorySnapshot {
            thread_id: thread.to_owned(),
            items: visible,
            source_model,
        })
    }

    /// Resolve the lineage segments for a paginated rollout, child last.
    ///
    /// The child rollout is segment zero; each `history_base` pointer walks
    /// one ancestor older. Cycles and unbounded chains are rejected; a
    /// missing ancestor surfaces as `source_missing` because the visible
    /// history would be silently incomplete otherwise.
    fn lineage_segments(
        &self,
        database: &Path,
        path: &Path,
        thread: &str,
    ) -> Result<Vec<LineageSegment>, HistoryError> {
        let location = locate(database, thread)?;
        let mut segments = vec![LineageSegment {
            thread: thread.to_owned(),
            location,
            bound: None,
            byte_offset: 0,
        }];
        let mut visited = vec![thread.to_owned()];
        let mut base = read_history_base(path)?;
        while let Some((parent_id, bound, byte_offset)) = base {
            if visited.contains(&parent_id) || segments.len() >= 32 {
                return Err(HistoryError::new("lineage_cycle"));
            }
            visited.push(parent_id.clone());
            let parent_location = self.locate_parent(database, &parent_id)?;
            base = read_history_base(&parent_location.path)?;
            segments.push(LineageSegment {
                thread: parent_id,
                location: parent_location,
                bound: Some(bound),
                byte_offset,
            });
        }
        // Chain was collected child-first; replay order is oldest first.
        segments.reverse();
        Ok(segments)
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
                // SQLite -wal/-shm sidecars share the `state_N` stem but are
                // not queryable databases; only the exact `state_N.sqlite`
                // file can answer the thread lookup.
                if path.extension().and_then(|extension| extension.to_str()) != Some("sqlite") {
                    return None;
                }
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
        replay: ReplayContext<'_>,
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
        // A lineage ancestor's `history_base.end_byte_offset` freezes the
        // physical prefix the child forked from. A file shorter than that
        // offset was truncated or rewritten: the frozen prefix contract is
        // broken and replaying the remainder would be silently wrong.
        let byte_cap = if replay.lineage_byte_offset > 0 {
            if replay.lineage_byte_offset > captured_end {
                return Err(HistoryError::new("lineage_prefix_truncated"));
            }
            replay.lineage_byte_offset
        } else {
            captured_end
        };

        let expected_thread = anchor.thread_id.as_deref();
        // Reverse base applies only to paginated resumes: the newest
        // replacement-history compaction becomes the history base and replay
        // covers its suffix. Fork checkpoints (exact compaction) keep the full
        // scan because ambiguity detection must see every matching record.
        let reverse_base = if replay.exact_compaction.is_none()
            && location.mode == "paginated"
            // A byte-capped ancestor never resumes from a reverse base: the
            // frozen prefix is a bounded replay, not a live resume.
            && byte_cap == captured_end
        {
            locate_latest_compaction(&mut file, captured_end, &location.mode)?
        } else {
            None
        };
        if let Some(base) = reverse_base {
            verify_session_thread(&mut file, captured_end, expected_thread)?;
            let suffix_start = base.suffix_start;
            // Anchor inside the suffix and unchanged file: replay from base.
            // Anchor outside the suffix or malformed suffix: full scan.
            let frame = ReplayFrame {
                start: suffix_start,
                end: captured_end,
                seed_visible: Vec::new(),
                seed_turn: base.turn.clone(),
                seed_successful: base.successful.clone(),
                seed_roles: base.response_roles.clone(),
            };
            let unchanged = file
                .metadata()
                .map_err(|_| HistoryError::new("source_unavailable"))?
                .len()
                >= captured_end;
            if unchanged && let SuffixScan::Usable(scan) = scan_suffix(&mut file, &frame, anchor)? {
                let snapshot = snapshot_from_base(
                    &mut file,
                    base,
                    *scan,
                    frame,
                    anchor,
                    replay.exact_compaction.is_some(),
                    replay.lineage_bound,
                )?;
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
        }

        // Reverse probing may leave the handle mid-file; restart at the top.
        file.seek(SeekFrom::Start(0))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        let scan = scan_rollout(&mut file, byte_cap, anchor, replay.exact_compaction)?;
        if file
            .metadata()
            .map_err(|_| HistoryError::new("source_unavailable"))?
            .len()
            < captured_end
        {
            return Err(HistoryError::new("source_changed"));
        }

        file.seek(SeekFrom::Start(0))
            .map_err(|_| HistoryError::new("source_unavailable"))?;
        let frame = ReplayFrame {
            end: byte_cap,
            seed_visible: replay.seed,
            ..ReplayFrame::head(byte_cap)
        };
        let mut scan = scan;
        if replay.exact_compaction.is_some()
            && let Some(turn) = scan.last_turn.clone()
        {
            scan.successful.insert(turn, true);
        }
        let visible = replay_records(&mut file, &frame, &scan, anchor, replay.lineage_bound)?;
        if location.mode == "paginated" {
            if !scan.saw_ordinal {
                return Err(HistoryError::new("ordinal_missing"));
            }
            // Any parsed record without the ordinal Codex promises is
            // malformed pagination, even when other records carry ordinals.
            if scan.ordinal_missing {
                return Err(HistoryError::new("ordinal_missing"));
            }
            if scan.ordinal_regressed {
                return Err(HistoryError::new("ordinal_regressed"));
            }
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

    walk_records(file, captured_end, |record| {
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
                observe_scan_record(&mut scan, &record, explicit_turn, turn);
                track_ordinal(&mut scan, &record);
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

/// Walk every parsed record in the byte range from the current seek position
/// to `end`, feeding each to `visit`. `Ok(false)` from `visit` stops the walk.
fn walk_records(
    file: &mut File,
    end: u64,
    mut visit: impl FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
) -> Result<(), HistoryError> {
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(end));
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
        let mut snapshot = self.read_rollout(
            &parent_anchor,
            &location,
            ReplayContext {
                exact_compaction: Some(compaction),
                ..ReplayContext::resume()
            },
        )?;
        snapshot.thread_id = anchor.thread_id.clone().unwrap_or_default();
        Ok(snapshot)
    }
}
