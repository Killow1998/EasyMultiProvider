//! Rollout identity, turn boundaries and completion evidence; no file I/O.
use super::records::{replacement_contains_encoded, session_meta_history_mode, session_meta_id};
use super::source::RecordSource;
use super::visible::{message_role, ordinal, record_turn_id, string_from, token};
use emp_history::{HistoryAnchor, HistoryError};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub(super) struct RolloutScan {
    pub(super) thread_ids: BTreeSet<String>,
    pub(super) history_modes: BTreeSet<String>,
    pub(super) successful: BTreeMap<String, bool>,
    pub(super) successful_models: Vec<(String, Option<String>)>,
    pub(super) response_roles: BTreeSet<(Option<String>, String)>,
    pub(super) saw_ordinal: bool,
    /// Newest ordinal observed on a paginated rollout, for monotonicity.
    pub(super) last_ordinal: Option<u64>,
    /// An ordinal regressed below the newest one: malformed pagination.
    pub(super) ordinal_regressed: bool,
    /// A record inside the replay window lacks the ordinal Codex promises.
    pub(super) ordinal_missing: bool,
    pub(super) explicit_turns: BTreeSet<String>,
    pub(super) terminal_turns: BTreeSet<String>,
    pub(super) boundary: usize,
    pub(super) total_records: usize,
    pub(super) exact_matches: usize,
    pub(super) exact_boundary: usize,
    pub(super) anchor_boundary: Option<usize>,
    pub(super) last_turn: Option<String>,
    pub(super) history_closed: bool,
}

/// Track paginated ordinal monotonicity while scanning.
///
/// A missing ordinal on an otherwise parsed record, or an ordinal that
/// regresses below the newest one, marks malformed pagination (checked after
/// the scan). Forward gaps stay legal: unknown or future records may skip.
pub(super) fn track_ordinal(scan: &mut RolloutScan, record: &Map<String, Value>) {
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

pub(super) fn observe_scan_record(
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

impl RolloutScan {
    pub(super) fn validate_pagination(&self, mode: &str) -> Result<(), HistoryError> {
        if mode != "paginated" {
            return Ok(());
        }
        if !self.saw_ordinal || self.ordinal_missing {
            return Err(HistoryError::new("ordinal_missing"));
        }
        if self.ordinal_regressed {
            return Err(HistoryError::new("ordinal_not_monotonic"));
        }
        Ok(())
    }

    pub(super) fn source_model(&self) -> Option<String> {
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

pub(super) fn scan_rollout(
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

pub(super) fn validate_scan_thread(
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
