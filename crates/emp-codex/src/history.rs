//! Bounded, read-only reconstruction of Codex-visible rollout history.

use emp_history::{HistoryAnchor, HistoryError, HistoryReader, HistorySnapshot, VisibleItem};
use records::{
    HistoryBase, locate, session_meta_history_base, session_meta_history_mode, session_meta_id,
};
use serde_json::{Map, Value};
use std::path::PathBuf;
use visible::{token, uuid_shape};

mod checkpoint;
mod lines;
mod location;
mod read;
mod records;
mod replay;
mod reverse;
mod scan;
mod source;
pub mod tool_timing;
mod visible;

#[path = "history_repair.rs"]
pub mod repair;

const MAX_LINEAGE_SEGMENTS: usize = 32;
const MAX_ROLLOUT_LINE_BYTES: usize = 64 * 1024 * 1024;
const MAX_ROLLOUT_SCAN_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const REVERSE_SCAN_WINDOW_CAP: u64 = 64 * 1024 * 1024;

struct ReplayContext<'a> {
    exact_compaction: Option<&'a Map<String, Value>>,
    lineage_bound: Option<u64>,
    lineage_byte_offset: u64,
    seed: Vec<VisibleItem>,
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

    /// With `force_full`, bypass reverse-compaction replay.
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
        let segments = self.lineage_segments(&database, location, thread)?;
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
}

impl HistoryReader for CodexHomeHistoryReader {
    fn read_compaction_history(
        &self,
        anchor: &HistoryAnchor,
        compaction: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError> {
        let (source_thread, resumed) = match self.read_visible_history(anchor) {
            Ok(snapshot) => (
                anchor
                    .thread_id
                    .as_deref()
                    .ok_or_else(|| HistoryError::new("thread_identity_missing"))?,
                Some(snapshot),
            ),
            Err(error)
                if !matches!(error.reason(), "thread_missing" | "thread_mismatch")
                    || anchor.forked_from_thread_id.is_none() =>
            {
                return Err(error);
            }
            Err(_) => {
                let parent = anchor
                    .forked_from_thread_id
                    .as_deref()
                    .ok_or_else(|| HistoryError::new("thread_missing"))?;
                if parent == anchor.thread_id.as_deref().unwrap_or_default() || !uuid_shape(parent)
                {
                    return Err(HistoryError::new("fork_parent_invalid"));
                }
                (parent, None)
            }
        };
        let database = self.latest_state_database()?;
        let location = locate(&database, source_thread)?;
        let legacy = location.mode == "legacy";
        let mut snapshot =
            match self.read_committed_checkpoint(&database, location, source_thread, compaction) {
                Ok(snapshot) => snapshot,
                Err(error) if legacy && error.reason() == "compaction_identity_missing" => {
                    match resumed.filter(|snapshot| {
                        snapshot.items.iter().any(|item| {
                            matches!(
                                item.kind.as_str(),
                                "compaction_summary" | "compaction_marker"
                            )
                        })
                    }) {
                        Some(snapshot) => snapshot,
                        None => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            };
        snapshot.thread_id = anchor.thread_id.clone().unwrap_or_default();
        Ok(snapshot)
    }
}
