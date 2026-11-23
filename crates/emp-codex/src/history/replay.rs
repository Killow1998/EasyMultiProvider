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

pub(super) struct ReplayFrame {
    pub(super) start: u64,
    /// Exclusive end offset (`captured_end` or a lineage byte cap).
    pub(super) end: u64,
    pub(super) seed_visible: Vec<VisibleItem>,
    /// Turn owning the seed, carried into records without a turn id.
    pub(super) seed_turn: Option<String>,
    /// Highest ordinal before `start`; `None` starts ordinal tracking fresh.
    pub(super) seed_ordinal: Option<u64>,
    pub(super) seed_successful: BTreeMap<String, bool>,
    /// Prefix model contexts must resolve the source model exactly as in full replay.
    pub(super) seed_models: Vec<(String, Option<String>)>,
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

pub(super) enum SuffixScan {
    Usable(Box<RolloutScan>),
    FallBack,
}

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
        track_ordinal(&mut scan, &record);
        if scan.history_closed {
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
        scan.anchor_boundary.is_some()
    } else {
        scan.explicit_turns.is_subset(&scan.terminal_turns)
    };
    if !usable {
        return Ok(SuffixScan::FallBack);
    }
    if !scan.saw_ordinal || scan.ordinal_missing || scan.ordinal_regressed {
        return Ok(SuffixScan::FallBack);
    }
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

pub(super) fn replay_records(
    source: &mut impl RecordSource,
    frame: &ReplayFrame,
    scan: &RolloutScan,
    anchor: &HistoryAnchor,
    lineage_bound: Option<u64>,
) -> Result<Vec<VisibleItem>, HistoryError> {
    let mut visible = frame.seed_visible.clone();
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
            return Ok(false);
        }
        let explicit_turn = record_turn_id(&record);
        if explicit_turn.is_some() {
            active_turn = explicit_turn.clone();
        }
        let turn = explicit_turn.clone().or_else(|| active_turn.clone());
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
    let items = replay_records(&mut source, &frame, &scan, anchor, None)?;
    Ok(HistorySnapshot {
        thread_id: anchor.thread_id.clone().unwrap_or_default(),
        items,
        source_model: scan.source_model(),
    })
}
