//! Visible replay and suffix fast path over an already bounded record source.
use super::records::{
    replacement_entries, replacement_history, session_meta_history_mode, session_meta_id,
};
use super::reverse::ReverseBase;
use super::scan::{RolloutScan, observe_scan_record, track_ordinal, validate_scan_thread};
use super::source::{FileRecordSource, RecordSource};
use super::visible::{
    apply_thread_rollback, message_role, normalize_visible_item, ordinal, record_turn_id, token,
    visible_payload,
};
use emp_history::{HistoryAnchor, HistoryError, HistorySnapshot, VisibleItem};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;

/// One replay pass over a rollout byte range.
///
/// The reverse-base path starts after a compaction record and seeds the replay
/// with that record's replacement history plus the pre-base turn state the
/// suffix scan cannot see. Every other path replays from the file head with an
/// empty seed.
pub(super) struct ReplayFrame {
    /// First replayed byte offset.
    pub(super) start: u64,
    /// Exclusive end offset (`captured_end` or a lineage byte cap).
    pub(super) end: u64,
    /// Visible items rebuilt before the first replayed record.
    pub(super) seed_visible: Vec<VisibleItem>,
    /// Turn owning the seed, carried into records without a turn id.
    pub(super) seed_turn: Option<String>,
    /// Highest ordinal before `start`; `None` starts ordinal tracking fresh.
    pub(super) seed_ordinal: Option<u64>,
    /// Turn completion states observed before `start`.
    pub(super) seed_successful: BTreeMap<String, bool>,
    /// (turn, model) contexts observed before `start`; the replay resolves
    /// its source model from these exactly as the full scan resolves its own.
    pub(super) seed_models: Vec<(String, Option<String>)>,
    /// Response roles observed before `start`.
    pub(super) seed_roles: BTreeSet<(Option<String>, String)>,
    pub(super) seed_thread_ids: BTreeSet<String>,
    pub(super) seed_history_modes: BTreeSet<String>,
}

impl ReplayFrame {
    pub(super) fn head(end: u64) -> Self {
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
pub(super) enum SuffixScan {
    Usable(Box<RolloutScan>),
    FallBack,
}

/// Forward scan of the records after a reverse-located compaction base.
pub(super) fn scan_suffix(
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
/// Replay the frame's records into `visible`, applying rollback markers, the
/// failed-turn filter, the response-role echo filter, and replacement-history
/// compaction records. Returns `None` when the boundary was hit and replay
/// must stop.
pub(super) fn replay_records(
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
pub(super) fn snapshot_from_base(
    file: &mut File,
    base: ReverseBase,
    scan: RolloutScan,
    frame: ReplayFrame,
    anchor: &HistoryAnchor,
) -> Result<HistorySnapshot, HistoryError> {
    let mut frame = frame;
    frame.seed_visible = replacement_entries(base.replacement, &[], base.turn.clone())?;
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
