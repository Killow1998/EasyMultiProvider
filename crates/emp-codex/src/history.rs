//! Bounded, read-only reconstruction of Codex-visible rollout history.

use emp_history::{HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

mod records;
mod source;
mod visible;

#[path = "history_repair.rs"]
pub mod repair;

use records::*;
use source::{FileRecordSource, RecordSource, WalkReport, ZstdRecordSource};
use visible::*;

const MAX_ROLLOUT_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ROLLOUT_SCAN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const REVERSE_SCAN_WINDOW_CAP: u64 = 64 * 1024 * 1024;
const MAX_LINEAGE_SEGMENTS: usize = 32;
const MAX_SESSION_DIRECTORY_DEPTH: usize = 8;

/// Replay wiring for one rollout read: lineage bounds, fork checkpoint, and
/// the seed history.
struct ReplayContext<'a> {
    exact_compaction: Option<&'a Map<String, Value>>,
    lineage_bound: Option<u64>,
    lineage_byte_offset: u64,
    seed: Vec<VisibleItem>,
    /// `true` disables the reverse-base fast path; production always uses
    /// `false` (auto). Exposed only for same-input fast/full parity tests.
    force_full: bool,
}

impl<'a> ReplayContext<'a> {
    fn resume() -> Self {
        Self {
            exact_compaction: None,
            lineage_bound: None,
            lineage_byte_offset: 0,
            seed: Vec::new(),
            force_full: false,
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
    history_modes: BTreeSet<String>,
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
    /// File offset of the compaction record, used to carry prefix controls.
    record_start: u64,
    /// File offset of the first record after the compaction record.
    suffix_start: u64,
    /// Explicit turn on the compaction record, if present.
    turn: Option<String>,
    /// Ordinal of the compaction record; seeds suffix monotonicity.
    ordinal: Option<u64>,
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
        let mut candidate: Option<ReverseBase> = None;
        let mut offset = line_start;
        loop {
            let Some(terminated) = read_bounded_line(&mut reader, &mut line)? else {
                break;
            };
            let record_start = offset;
            offset += line.len() as u64;
            match rollout_json_line(&line, terminated) {
                Ok(Some(record)) => {
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
                        let base_ordinal = ordinal(&record);
                        // A paginated base without an ordinal cannot seed the
                        // suffix monotonicity check; the full scan owns it.
                        let has_seed_ordinal =
                            history_mode != "paginated" || base_ordinal.is_some();
                        candidate = (eligible && has_seed_ordinal).then(|| ReverseBase {
                            replacement: replacement.unwrap_or_default(),
                            record_start,
                            suffix_start: offset,
                            turn: record_turn_id(&record),
                            ordinal: base_ordinal,
                        });
                    }
                }
                Ok(None) => {}
                // Malformed complete lines are reported by the full scan.
                Err(HistoryError { .. }) if terminated => return Ok(None),
                Err(error) => return Err(error),
            }
        }
        if candidate.is_some() {
            return Ok(candidate);
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

/// Read the lineage pointer from the rollout's first session meta record.
///
/// Returns `None` for rollouts without a `history_base`, which covers every
/// rollout written before Codex paginated persistent forks.
fn read_session_meta(path: &Path) -> Result<Option<Map<String, Value>>, HistoryError> {
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(HistoryError::new("source_missing"));
    }
    let mut file = File::open(path).map_err(|_| HistoryError::new("source_missing"))?;
    let captured_end = file
        .metadata()
        .map_err(|_| HistoryError::new("source_unavailable"))?
        .len();
    if is_compressed_rollout(path) {
        if captured_end > MAX_ROLLOUT_SCAN_BYTES {
            return Err(HistoryError::new("source_too_large"));
        }
        let compressed = file.by_ref().take(captured_end);
        let decoder = zstd::stream::read::Decoder::new(compressed)
            .map_err(|_| HistoryError::new("invalid_compressed_rollout"))?;
        let mut reader = BufReader::with_capacity(
            64 * 1024,
            decoder.take((MAX_ROLLOUT_LINE_BYTES as u64).saturating_add(1)),
        );
        return read_session_meta_from(&mut reader, "invalid_compressed_rollout");
    }
    let head = captured_end.min((MAX_ROLLOUT_LINE_BYTES as u64).saturating_add(1));
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(head));
    read_session_meta_from(&mut reader, "source_unavailable")
}

fn read_session_meta_from(
    reader: &mut impl BufRead,
    io_error_reason: &str,
) -> Result<Option<Map<String, Value>>, HistoryError> {
    let mut line = Vec::new();
    while let Some(terminated) = read_bounded_line_with_reason(reader, &mut line, io_error_reason)?
    {
        let Some(record) = rollout_json_line(&line, terminated)? else {
            continue;
        };
        if matches!(
            token(record.get("type")).as_str(),
            "session_meta" | "sessionmeta"
        ) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

fn read_history_base(path: &Path) -> Result<Option<HistoryBase>, HistoryError> {
    read_session_meta(path)?
        .as_ref()
        .map(session_meta_history_base)
        .transpose()
        .map(Option::flatten)
}

fn is_compressed_rollout(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with(".jsonl.zst"))
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
    /// Highest ordinal before `start`; `None` starts ordinal tracking fresh.
    seed_ordinal: Option<u64>,
    /// Turn completion states observed before `start`.
    seed_successful: BTreeMap<String, bool>,
    /// (turn, model) contexts observed before `start`; the replay resolves
    /// its source model from these exactly as the full scan resolves its own.
    seed_models: Vec<(String, Option<String>)>,
    /// Response roles observed before `start`.
    seed_roles: BTreeSet<(Option<String>, String)>,
    seed_thread_ids: BTreeSet<String>,
    seed_history_modes: BTreeSet<String>,
}

impl ReplayFrame {
    fn head(end: u64) -> Self {
        Self {
            start: 0,
            end,
            seed_visible: Vec::new(),
            seed_turn: None,
            seed_ordinal: None,
            seed_successful: BTreeMap::new(),
            seed_models: Vec::new(),
            seed_roles: BTreeSet::new(),
            seed_thread_ids: BTreeSet::new(),
            seed_history_modes: BTreeSet::new(),
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
    expected_mode: &str,
) -> Result<SuffixScan, HistoryError> {
    let mut source = FileRecordSource::new(file, frame.start, frame.end);
    let mut scan = RolloutScan {
        last_ordinal: frame.seed_ordinal,
        saw_ordinal: frame.seed_ordinal.is_some(),
        successful_models: frame.seed_models.clone(),
        successful: frame.seed_successful.clone(),
        thread_ids: frame.seed_thread_ids.clone(),
        history_modes: frame.seed_history_modes.clone(),
        ..RolloutScan::default()
    };
    let mut active_turn = frame.seed_turn.clone();
    let mut index = 0usize;
    source.walk(&mut |record| {
        if matches!(
            token(record.get("type")).as_str(),
            "session_meta" | "sessionmeta"
        ) {
            if let Some(id) = session_meta_id(&record) {
                scan.thread_ids.insert(id);
            }
            if let Some(mode) = session_meta_history_mode(&record)? {
                scan.history_modes.insert(mode);
            }
        }
        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());
        // Ordinal integrity covers every parsed record, including the
        // anchor hit itself; skipping it would let a suffix that contains
        // only the anchor pass off a missing-ordinal defect.
        track_ordinal(&mut scan, &record);
        if scan.history_closed {
            // The full scan stops observing once the anchor closes history;
            // semantic state after the boundary (model contexts, roles,
            // completion states) must not influence the fast replay either.
        } else if anchor
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
    validate_scan_thread(&scan, anchor.thread_id.as_deref(), expected_mode)?;
    let usable = if anchor.turn_id.is_some() {
        // Anchor turn predates the compaction base: full scan required.
        scan.anchor_boundary.is_some()
    } else {
        scan.explicit_turns.is_subset(&scan.terminal_turns)
    };
    if !usable {
        return Ok(SuffixScan::FallBack);
    }
    // The full scan reports paginated ordinal defects terminally; the suffix
    // scan only ever sees part of the file, so any defect it does see means
    // the base cannot be trusted and the full scan must own the outcome.
    if !scan.saw_ordinal || scan.ordinal_missing || scan.ordinal_regressed {
        return Ok(SuffixScan::FallBack);
    }
    // A replacement is excluded from history by the failed-turn filter
    // unless its owning turn completed successfully; a turn with no
    // task_complete at all (interrupted) is unsuccessful too. A base owned
    // by any non-successful turn would seed visible items full replay
    // would never produce.
    if let Some(turn) = &frame.seed_turn {
        let outcome = scan
            .successful
            .get(turn)
            .or_else(|| frame.seed_successful.get(turn));
        if outcome != Some(&true) {
            return Ok(SuffixScan::FallBack);
        }
    }
    scan.boundary = scan.anchor_boundary.unwrap_or(scan.total_records);
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
    source: &mut impl RecordSource,
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
    let mut active_turn = frame.seed_turn.clone();
    let mut index = 0usize;
    source.walk(&mut |record| {
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
fn snapshot_from_base(
    file: &mut File,
    base: ReverseBase,
    scan: RolloutScan,
    frame: ReplayFrame,
    anchor: &HistoryAnchor,
) -> Result<HistorySnapshot, HistoryError> {
    let mut frame = frame;
    frame.seed_visible = replacement_entries(
        base.replacement,
        &Vec::<VisibleItem>::new(),
        base.turn.clone(),
    )?;
    let mut source = FileRecordSource::new(file, frame.start, frame.end);
    // The reverse base never serves a lineage-bounded or fork-checkpoint
    // replay, so the suffix replays unbounded.
    let items = replay_records(&mut source, &frame, &scan, anchor, None)?;
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
        self.read_visible_history_with_strategy(anchor, false)
    }

    /// Test seam: replays the same durable rollout with the reverse-base
    /// fast path disabled, so the differential contract suite can prove the
    /// two strategies agree on byte-identical inputs. Production callers
    /// always use [`Self::read_visible_history`].
    #[doc(hidden)]
    pub fn read_visible_history_with_strategy(
        &self,
        anchor: &HistoryAnchor,
        force_full: bool,
    ) -> Result<HistorySnapshot, HistoryError> {
        let thread = anchor
            .thread_id
            .as_deref()
            .ok_or_else(|| HistoryError::new("thread_identity_missing"))?;
        let database = self.latest_state_database()?;
        let location = locate(&database, thread)?;
        if location.mode != "paginated" {
            return self.read_rollout(
                anchor,
                &location,
                ReplayContext {
                    force_full,
                    ..ReplayContext::resume()
                },
            );
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
                    force_full,
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
        let mut next_base = read_history_base(&self.contained_rollout_path(path)?)?;
        while let Some(base) = next_base {
            if visited.contains(&base.thread_id) {
                return Err(HistoryError::new("lineage_cycle"));
            }
            if segments.len() >= MAX_LINEAGE_SEGMENTS {
                return Err(HistoryError::new("lineage_depth_exceeded"));
            }
            let parent_id = base.thread_id;
            let bound = base.end_ordinal_exclusive;
            let byte_offset = base.end_byte_offset;
            visited.push(parent_id.clone());
            let parent_location = self.locate_parent(database, &parent_id)?;
            next_base = read_history_base(&parent_location.path)?;
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
        let roots = self.canonical_session_roots()?;
        match locate(database, rollout_id) {
            Ok(location) => {
                if self.is_under_session_root(&location.path, &roots) {
                    return Ok(location);
                }
                return Err(HistoryError::new("rollout_outside_session_root"));
            }
            Err(error) if error.reason() != "thread_missing" => return Err(error),
            Err(_) => {}
        }
        let expected = [
            format!("{rollout_id}.jsonl"),
            format!("{rollout_id}.jsonl.zst"),
        ];
        let mut found = BTreeSet::new();
        for root in &roots {
            let mut stack = vec![(root.clone(), 0usize)];
            while let Some((directory, depth)) = stack.pop() {
                let metadata = match fs::symlink_metadata(&directory) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    continue;
                }
                let entries = match fs::read_dir(&directory) {
                    Ok(entries) => entries,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(_) => continue,
                    };
                    if file_type.is_symlink() {
                        continue;
                    }
                    if file_type.is_dir() {
                        if depth < MAX_SESSION_DIRECTORY_DEPTH {
                            stack.push((path, depth + 1));
                        }
                        continue;
                    }
                    if file_type.is_file()
                        && path
                            .file_name()
                            .and_then(OsStr::to_str)
                            .is_some_and(|name| expected.iter().any(|candidate| candidate == name))
                        && let Ok(canonical) = path.canonicalize()
                        && canonical.starts_with(root)
                    {
                        found.insert(canonical);
                    }
                }
            }
        }
        match found.len() {
            0 => Err(HistoryError::new("source_missing")),
            1 => {
                let path = found.into_iter().next().expect("one parent file");
                let meta = read_session_meta(&path)?
                    .ok_or_else(|| HistoryError::new("session_meta_missing"))?;
                let id = session_meta_id(&meta)
                    .ok_or_else(|| HistoryError::new("session_meta_missing"))?;
                if id != rollout_id {
                    return Err(HistoryError::new("thread_mismatch"));
                }
                let mode =
                    session_meta_history_mode(&meta)?.unwrap_or_else(|| "paginated".to_owned());
                Ok(Location {
                    path,
                    mode,
                    source_model: None,
                })
            }
            _ => Err(HistoryError::new("lineage_source_ambiguous")),
        }
    }

    fn canonical_session_roots(&self) -> Result<Vec<PathBuf>, HistoryError> {
        let home = self
            .home
            .canonicalize()
            .map_err(|_| HistoryError::new("history_unavailable"))?;
        let mut roots = Vec::new();
        for name in ["sessions", "archived_sessions"] {
            let path = home.join(name);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            if let Ok(canonical) = path.canonicalize()
                && canonical.starts_with(&home)
            {
                roots.push(canonical);
            }
        }
        Ok(roots)
    }

    fn is_under_session_root(&self, path: &Path, roots: &[PathBuf]) -> bool {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(directory) => directory.join(path),
                Err(_) => return false,
            }
        };
        let Ok(canonical) = absolute.canonicalize() else {
            return false;
        };
        if !canonical.is_file() {
            return false;
        }
        roots.iter().any(|root| {
            if !canonical.starts_with(root) {
                return false;
            }
            // `absolute` may reach the root through a symlinked or otherwise
            // non-canonical home; strip the ancestor that resolves to the
            // root, then require every component below it to be a real entry.
            let Some(relative) = absolute.ancestors().skip(1).find_map(|ancestor| {
                (ancestor.canonicalize().ok()? == *root)
                    .then(|| absolute.strip_prefix(ancestor).ok())
                    .flatten()
            }) else {
                return false;
            };
            let mut current = root.clone();
            for component in relative.components() {
                let Component::Normal(name) = component else {
                    return false;
                };
                current.push(name);
                let Ok(metadata) = fs::symlink_metadata(&current) else {
                    return false;
                };
                if metadata.file_type().is_symlink() {
                    return false;
                }
            }
            true
        })
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

    /// Canonical rollout path, provided it is a regular file inside the
    /// Codex home. Checked before any open so a database row cannot point a
    /// read at a FIFO or at a file outside the home.
    fn contained_rollout_path(&self, path: &Path) -> Result<PathBuf, HistoryError> {
        let home = self
            .home
            .canonicalize()
            .map_err(|_| HistoryError::new("history_unavailable"))?;
        let path = path
            .canonicalize()
            .map_err(|_| HistoryError::new("source_missing"))?;
        if !path.starts_with(&home) {
            return Err(HistoryError::new("rollout_outside_codex_home"));
        }
        let metadata = fs::metadata(&path).map_err(|_| HistoryError::new("source_missing"))?;
        if !metadata.is_file() {
            return Err(HistoryError::new("source_missing"));
        }
        Ok(path)
    }

    fn read_rollout(
        &self,
        anchor: &HistoryAnchor,
        location: &Location,
        replay: ReplayContext<'_>,
    ) -> Result<HistorySnapshot, HistoryError> {
        let path = self.contained_rollout_path(&location.path)?;
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
        if is_compressed_rollout(&path) {
            return self.read_compressed_rollout(anchor, location, file, captured_end, replay);
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

        // Reverse base applies only to paginated resumes: the newest
        // replacement-history compaction becomes the history base and replay
        // covers its suffix. Fork checkpoints (exact compaction) keep the full
        // scan because ambiguity detection must see every matching record.
        let reverse_base = if replay.exact_compaction.is_none()
            && !replay.force_full
            && location.mode == "paginated"
            // A byte-capped ancestor never resumes from a reverse base: the
            // frozen prefix is a bounded replay, not a live resume.
            && byte_cap == captured_end
            // Neither does an ordinal-bounded ancestor: the newest checkpoint
            // may postdate the fork and would seed history the bound excludes.
            && replay.lineage_bound.is_none()
        {
            locate_latest_compaction(&mut file, captured_end, &location.mode)?
        } else {
            None
        };
        if let Some(mut base) = reverse_base {
            // A checkpoint only replaces visible items. Scan the complete
            // prefix for control state so older turns, models, response-role
            // deduplication, identity/mode, and ordinals survive the base.
            let prefix_scan = {
                let prefix_anchor = HistoryAnchor {
                    thread_id: anchor.thread_id.clone(),
                    ..HistoryAnchor::default()
                };
                let mut source = FileRecordSource::new(&mut file, 0, base.record_start);
                scan_rollout(&mut source, &prefix_anchor, None, None, &location.mode)?
            };
            if location.mode == "paginated" {
                if !prefix_scan.saw_ordinal || prefix_scan.ordinal_missing || base.ordinal.is_none()
                {
                    return Err(HistoryError::new("ordinal_missing"));
                }
                if prefix_scan.ordinal_regressed
                    || prefix_scan
                        .last_ordinal
                        .is_some_and(|last| base.ordinal.is_some_and(|base| base < last))
                {
                    return Err(HistoryError::new("ordinal_not_monotonic"));
                }
            }
            let suffix_start = base.suffix_start;
            let seed_turn = base.turn.clone().or(prefix_scan.last_turn.clone());
            base.turn = seed_turn.clone();
            // Anchor inside the suffix and unchanged file: replay from base.
            // Anchor outside the suffix or malformed suffix: full scan.
            let frame = ReplayFrame {
                start: suffix_start,
                end: captured_end,
                seed_visible: Vec::new(),
                seed_turn,
                seed_ordinal: base.ordinal.or(prefix_scan.last_ordinal),
                seed_successful: prefix_scan.successful,
                seed_models: prefix_scan.successful_models,
                seed_roles: prefix_scan.response_roles,
                seed_thread_ids: prefix_scan.thread_ids,
                seed_history_modes: prefix_scan.history_modes,
            };
            let unchanged = file
                .metadata()
                .map_err(|_| HistoryError::new("source_unavailable"))?
                .len()
                >= captured_end;
            if unchanged
                && let SuffixScan::Usable(scan) =
                    scan_suffix(&mut file, &frame, anchor, &location.mode)?
            {
                let snapshot = snapshot_from_base(&mut file, base, *scan, frame, anchor)?;
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
        let scan = {
            let mut source = FileRecordSource::new(&mut file, 0, byte_cap);
            scan_rollout(
                &mut source,
                anchor,
                replay.exact_compaction,
                replay.lineage_bound,
                &location.mode,
            )?
        };
        if file
            .metadata()
            .map_err(|_| HistoryError::new("source_unavailable"))?
            .len()
            < captured_end
        {
            return Err(HistoryError::new("source_changed"));
        }

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
        let mut source = FileRecordSource::new(&mut file, 0, byte_cap);
        let visible = replay_records(&mut source, &frame, &scan, anchor, replay.lineage_bound)?;
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
                return Err(HistoryError::new("ordinal_not_monotonic"));
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

    fn read_compressed_rollout(
        &self,
        anchor: &HistoryAnchor,
        location: &Location,
        mut file: File,
        compressed_end: u64,
        replay: ReplayContext<'_>,
    ) -> Result<HistorySnapshot, HistoryError> {
        let frozen_prefix = (replay.lineage_byte_offset > 0).then_some(replay.lineage_byte_offset);
        let scan = {
            let mut source = ZstdRecordSource::new(&mut file, compressed_end, frozen_prefix);
            scan_rollout(
                &mut source,
                anchor,
                replay.exact_compaction,
                replay.lineage_bound,
                &location.mode,
            )?
        };
        if file
            .metadata()
            .map_err(|_| HistoryError::new("source_unavailable"))?
            .len()
            < compressed_end
        {
            return Err(HistoryError::new("source_changed"));
        }
        let byte_limit = frozen_prefix.unwrap_or_else(|| MAX_ROLLOUT_SCAN_BYTES.saturating_add(1));
        let frame = ReplayFrame {
            end: byte_limit,
            seed_visible: replay.seed,
            ..ReplayFrame::head(byte_limit)
        };
        let mut scan = scan;
        if replay.exact_compaction.is_some()
            && let Some(turn) = scan.last_turn.clone()
        {
            scan.successful.insert(turn, true);
        }
        let visible = {
            let mut source = ZstdRecordSource::new(&mut file, compressed_end, frozen_prefix);
            replay_records(&mut source, &frame, &scan, anchor, replay.lineage_bound)?
        };
        if location.mode == "paginated" {
            if !scan.saw_ordinal || scan.ordinal_missing {
                return Err(HistoryError::new("ordinal_missing"));
            }
            if scan.ordinal_regressed {
                return Err(HistoryError::new("ordinal_not_monotonic"));
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
    source: &mut impl RecordSource,
    anchor: &HistoryAnchor,
    exact_compaction: Option<&Map<String, Value>>,
    lineage_bound: Option<u64>,
    expected_mode: &str,
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

    let mut lineage_boundary = None;
    source.walk(&mut |record| {
        let record_type = token(record.get("type"));
        if matches!(record_type.as_str(), "session_meta" | "sessionmeta") {
            if let Some(id) = session_meta_id(&record) {
                scan.thread_ids.insert(id);
            }
            if let Some(mode) = session_meta_history_mode(&record)? {
                scan.history_modes.insert(mode);
            }
        }

        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());

        let crosses_lineage_bound =
            lineage_bound.is_some_and(|bound| ordinal(&record).is_some_and(|value| value >= bound));
        if crosses_lineage_bound {
            lineage_boundary.get_or_insert(index);
        } else if lineage_boundary.is_none() {
            track_ordinal(&mut scan, &record);
        }

        if !crosses_lineage_bound && lineage_boundary.is_none() && !scan.history_closed {
            if anchor_turn.is_some_and(|turn| record_turn_id(&record).as_deref() == Some(turn)) {
                scan.anchor_boundary = Some(index);
                scan.boundary = index;
                scan.history_closed = true;
            } else {
                scan.last_turn = turn.clone();
                observe_scan_record(&mut scan, &record, explicit_turn, turn);
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
        } else if lineage_boundary.is_none()
            && exact_compaction.is_some()
            && replacement_contains_encoded(&record, exact_encoded.unwrap_or_default())
        {
            scan.exact_matches += 1;
        }

        index += 1;
        Ok(true)
    })?;

    scan.total_records = index;
    validate_scan_thread(&scan, expected_thread, expected_mode)?;
    scan.boundary = if exact_compaction.is_some() {
        match scan.exact_matches {
            1 => scan.exact_boundary,
            0 => return Err(HistoryError::new("compaction_identity_missing")),
            _ => return Err(HistoryError::new("compaction_identity_ambiguous")),
        }
    } else if anchor_turn.is_none() {
        lineage_boundary.unwrap_or(scan.total_records)
    } else if let Some(index) = scan.anchor_boundary {
        index
    } else if scan.explicit_turns.is_subset(&scan.terminal_turns) {
        lineage_boundary.unwrap_or(scan.total_records)
    } else {
        return Err(HistoryError::new("turn_not_found"));
    };
    Ok(scan)
}

fn validate_scan_thread(
    scan: &RolloutScan,
    expected: Option<&str>,
    expected_mode: &str,
) -> Result<(), HistoryError> {
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
    if scan.history_modes.iter().any(|mode| mode != expected_mode) {
        return Err(HistoryError::new("history_mode_mismatch"));
    }
    Ok(())
}

/// Walk every parsed record in the byte range from the current seek position
/// to `end`, feeding each to `visit`. `Ok(false)` from `visit` stops the walk.
fn walk_records(
    file: &mut File,
    end: u64,
    mut visit: impl FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
) -> Result<WalkReport, HistoryError> {
    let start = file
        .stream_position()
        .map_err(|_| HistoryError::new("source_unavailable"))?;
    if start > end {
        return Err(HistoryError::new("invalid_replay_range"));
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file.by_ref().take(end - start));
    walk_bounded_reader(&mut reader, end - start, "source_unavailable", &mut visit)
}

fn walk_bounded_reader(
    reader: &mut impl BufRead,
    end: u64,
    io_error_reason: &str,
    mut visit: impl FnMut(Map<String, Value>) -> Result<bool, HistoryError>,
) -> Result<WalkReport, HistoryError> {
    let mut line = Vec::new();
    let mut bytes_read = 0u64;
    while let Some(terminated) = read_bounded_line_with_reason(reader, &mut line, io_error_reason)?
    {
        bytes_read = bytes_read.saturating_add(line.len() as u64);
        if bytes_read > end {
            return Err(HistoryError::new("invalid_replay_range"));
        }
        if let Some(record) = rollout_json_line(&line, terminated)?
            && !visit(record)?
        {
            return Ok(WalkReport {
                bytes_read,
                stopped_early: true,
            });
        }
    }
    Ok(WalkReport {
        bytes_read,
        stopped_early: false,
    })
}

fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> Result<Option<bool>, HistoryError> {
    read_bounded_line_with_reason(reader, line, "source_unavailable")
}

fn read_bounded_line_with_reason(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    io_error_reason: &str,
) -> Result<Option<bool>, HistoryError> {
    line.clear();
    loop {
        let available = reader
            .fill_buf()
            .map_err(|_| HistoryError::new(io_error_reason))?;
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

#[cfg(test)]
mod walker_boundary_tests {
    use super::*;
    use std::io::Write;

    /// The walker's `end` is an absolute offset captured BEFORE the walk
    /// starts. Records appended after the capture (a concurrent writer) must
    /// never be observed, even when the handle's file grew behind the walk.
    #[test]
    fn walker_absolute_end_ignores_concurrent_append() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        std::fs::write(&path, "{\"ordinal\":1,\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first\"}}\n").unwrap();
        let mut file = File::open(&path).unwrap();
        let captured_end = file.metadata().unwrap().len();
        // Seek somewhere non-zero like the reverse probe does, then append.
        file.seek(SeekFrom::Start(4)).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                b"{\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"APPENDED\"}}\n",
            )
            .unwrap();
        // Rewind to a non-zero start and walk to the frozen absolute end.
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut seen = Vec::new();
        walk_records(&mut file, captured_end, |record| {
            seen.push(serde_json::to_string(&record).unwrap());
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen.len(), 1, "walker must stop at the frozen end");
        assert!(!seen[0].contains("APPENDED"), "{:?}", seen[0]);
    }
}
