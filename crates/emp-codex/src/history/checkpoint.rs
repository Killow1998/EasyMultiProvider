//! Recover the exact committed checkpoint over the same frozen lineage as resume.
use super::location::{LineageSegment, Location};
use super::{CodexHomeHistoryReader, ReplayContext};
use emp_history::{HistoryAnchor, HistoryError, HistorySnapshot};
use serde_json::{Map, Value};
use std::path::Path;

impl CodexHomeHistoryReader {
    pub(super) fn read_committed_checkpoint(
        &self,
        database: &Path,
        location: Location,
        thread: &str,
        compaction: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError> {
        let segments = if location.mode == "paginated" {
            self.lineage_segments(database, &location.path, thread)?
        } else {
            vec![LineageSegment {
                thread: thread.to_owned(),
                location,
                bound: None,
                byte_offset: 0,
            }]
        };
        let mut visible = Vec::new();
        let mut source_model = None;
        let mut committed = None;
        for segment in segments {
            let anchor = HistoryAnchor {
                thread_id: Some(segment.thread),
                ..HistoryAnchor::default()
            };
            // The same prefix bounds apply both when looking for the identity
            // and when replaying ancestors. Parent activity after the fork must
            // neither match a checkpoint nor become the child's seed history.
            let replay = ReplayContext {
                lineage_bound: segment.bound,
                lineage_byte_offset: segment.byte_offset,
                seed: visible,
                ..ReplayContext::resume()
            };
            match self.read_rollout(
                &anchor,
                &segment.location,
                ReplayContext {
                    exact_compaction: Some(compaction),
                    seed: replay.seed.clone(),
                    ..replay
                },
            ) {
                Ok(mut snapshot) => {
                    if committed.is_some() {
                        return Err(HistoryError::new("compaction_identity_ambiguous"));
                    }
                    snapshot.source_model = snapshot.source_model.or(source_model);
                    source_model = snapshot.source_model.clone();
                    visible = snapshot.items.clone();
                    committed = Some(snapshot);
                }
                Err(error) if error.reason() == "compaction_identity_missing" => {
                    if committed.is_none() {
                        let snapshot = self.read_rollout(&anchor, &segment.location, replay)?;
                        visible = snapshot.items;
                        source_model = snapshot.source_model.or(source_model);
                    } else {
                        // The client's tail already owns everything after the
                        // checkpoint. Remaining segments only check ambiguity.
                        visible = replay.seed;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        committed.ok_or_else(|| HistoryError::new("compaction_identity_missing"))
    }
}
