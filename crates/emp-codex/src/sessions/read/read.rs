use super::location::Location;
use super::replay::{ReplayFrame, SuffixScan, replay_records, scan_suffix, snapshot_from_base};
use super::reverse::locate_latest_compaction;
use super::scan::scan_rollout;
use super::source::{FileRecordSource, ZstdRecordSource, is_compressed_rollout};
use super::{CodexHomeHistoryReader, MAX_ROLLOUT_SCAN_BYTES, ReplayContext};
use emp_context::{HistoryAnchor, HistoryError, HistorySnapshot};
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
        let byte_cap = if replay.lineage_byte_offset > 0 {
            if replay.lineage_byte_offset > captured_end {
                return Err(HistoryError::new("lineage_prefix_truncated"));
            }
            replay.lineage_byte_offset
        } else {
            captured_end
        };

        let reverse_base = if replay.exact_compaction.is_none()
            && !replay.force_full
            && location.mode == "paginated"
            && byte_cap == captured_end
            && replay.lineage_bound.is_none()
        {
            locate_latest_compaction(&mut file, captured_end, &location.mode)?
        } else {
            None
        };
        if let Some(mut base) = reverse_base {
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
