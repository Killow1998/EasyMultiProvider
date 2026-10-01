//! Plain and compressed rollout orchestration; identity and replay live separately.
use super::location::Location;
use super::replay::{ReplayFrame, SuffixScan, replay_records, scan_suffix, snapshot_from_base};
use super::reverse::locate_latest_compaction;
use super::scan::scan_rollout;
use super::source::{FileRecordSource, ZstdRecordSource, is_compressed_rollout};
use super::{CodexHomeHistoryReader, MAX_ROLLOUT_SCAN_BYTES, ReplayContext};
use emp_history::{HistoryAnchor, HistoryError, HistorySnapshot};
use std::fs::File;
use std::io::{Seek, SeekFrom};

impl CodexHomeHistoryReader {
    pub(super) fn read_rollout(
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
        scan.validate_pagination(&location.mode)?;
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
        scan.validate_pagination(&location.mode)?;
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
