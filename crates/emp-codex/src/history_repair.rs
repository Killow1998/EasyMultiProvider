//! Reversible, append-only conversion of EMP portable checkpoints for native Codex.
use crate::history::uuid_shape;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

const REPAIR_DIRECTORY: &str = "easy-multi-provider/integration/history-repair";

#[path = "history_repair/checkpoint.rs"]
mod checkpoint;
#[path = "history_repair/coordination.rs"]
mod coordination;
#[path = "history_repair/inventory.rs"]
mod inventory;
#[path = "history_repair/paths.rs"]
mod paths;
#[path = "history_repair/planning.rs"]
mod planning;
#[path = "history_repair/replay.rs"]
mod replay;
#[path = "history_repair/selection.rs"]
mod selection;
#[path = "history_repair/spool.rs"]
mod spool;
#[path = "history_repair/transaction.rs"]
mod transaction;
pub use coordination::HistoryRepairLock;
use coordination::{
    COORDINATION_LOCK, LOCK_DIRECTORY, acquire_coordination_lock, probe_codex_writers,
};
use transaction::{pending_manifests, recover_pending};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct HistoryRepairReport {
    pub threads_repaired: usize,
    pub checkpoints_converted: usize,
    /// Confirmed EMP checkpoints whose conversion could not finish safely.
    pub threads_deferred: usize,
    /// Histories left untouched before confirming a conversion was needed.
    pub threads_skipped: usize,
    pub deferred_reasons: BTreeMap<String, usize>,
    pub skipped_reasons: BTreeMap<String, usize>,
}

impl HistoryRepairReport {
    fn skip(&mut self, reason: &str) {
        self.threads_skipped += 1;
        *self.skipped_reasons.entry(reason.to_owned()).or_default() += 1;
    }

    fn defer(&mut self, reason: &str) {
        self.threads_deferred += 1;
        *self.deferred_reasons.entry(reason.to_owned()).or_default() += 1;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRepairError {
    reason: &'static str,
}

impl HistoryRepairError {
    fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for HistoryRepairError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "native history repair failed: {}", self.reason)
    }
}

impl std::error::Error for HistoryRepairError {}

/// Convert EMP-owned `emp1:` compaction response items in every Codex thread
/// and existing paginated descendant. The returned lock must stay alive until
/// the native configuration restore has completed.
pub fn repair_before_native_restore(
    codex_home: &Path,
) -> Result<(HistoryRepairLock, HistoryRepairReport), HistoryRepairError> {
    repair_selected(codex_home, None, false)
}

/// Convert only idle, verifiably coordinated history. The returned lock also
/// protects the caller's configuration restore from newly starting writers.
pub fn repair_idle_before_native_restore(
    codex_home: &Path,
) -> Result<(HistoryRepairLock, HistoryRepairReport), HistoryRepairError> {
    repair_selected(codex_home, None, true)
}

/// Migrate only the selected legacy thread, including inherited context at its
/// fork cutoff. Other rollouts and their writer locks are not selected.
pub fn repair_thread_before_native_resume(
    codex_home: &Path,
    thread_id: &str,
) -> Result<(HistoryRepairLock, HistoryRepairReport), HistoryRepairError> {
    if !uuid_shape(thread_id) {
        return Err(HistoryRepairError::new("invalid_thread_id"));
    }
    repair_selected(codex_home, Some(thread_id), false)
}

fn repair_selected(
    codex_home: &Path,
    selected: Option<&str>,
    automatic: bool,
) -> Result<(HistoryRepairLock, HistoryRepairReport), HistoryRepairError> {
    let home = inventory::canonical_home(codex_home)?;
    if automatic && !home.join(LOCK_DIRECTORY).join(COORDINATION_LOCK).is_file() {
        if inventory::has_rollouts(&home)? {
            return Err(HistoryRepairError::new("writer_coordination_unavailable"));
        }
        // An empty home needs no migration lock. Do not manufacture evidence
        // that a future, possibly older Codex writer supports coordination.
        return Ok((
            HistoryRepairLock {
                _coordination: None,
            },
            HistoryRepairReport::default(),
        ));
    }
    let coordination = acquire_coordination_lock(&home, automatic)?;
    let repair_root = ensure_repair_root(&home)?;

    let Some(mut infos) = inventory::threads(&home, selected)? else {
        return Ok((
            HistoryRepairLock {
                _coordination: Some(coordination),
            },
            HistoryRepairReport::default(),
        ));
    };
    let mut pending = pending_manifests(&home, &repair_root)?;
    if let Some(selected) = selected {
        pending.retain(|(_, manifest)| {
            manifest
                .entries
                .iter()
                .any(|entry| entry.thread_id == selected)
        });
        if pending.iter().any(|(_, manifest)| {
            manifest
                .entries
                .iter()
                .any(|entry| entry.thread_id != selected)
        }) {
            return Err(HistoryRepairError::new(
                "pending_repair_includes_other_threads",
            ));
        }
    }
    let mut lock_ids = infos
        .iter()
        .map(|row| row.id.clone())
        .collect::<BTreeSet<_>>();
    for (_, manifest) in &pending {
        lock_ids.extend(manifest.entries.iter().map(|entry| entry.thread_id.clone()));
    }
    let mut active = probe_codex_writers(&home, &lock_ids, selected.is_none())?;
    if !automatic && !active.is_empty() {
        return Err(HistoryRepairError::new("active_codex_writer"));
    }
    let paths = paths::RolloutPaths::new(&home)?;
    let mut report = HistoryRepairReport::default();
    if automatic {
        // A transaction is indivisible, even if another interrupted manifest
        // shares one of its threads. Expand the blocked set before recovering.
        let mut blocked_reason = "active_codex_writer";
        loop {
            let before = active.len();
            if coordination::protect_active_lineages(&paths, &infos, &mut active).is_err() {
                active.extend(lock_ids.iter().cloned());
                blocked_reason = "active_lineage_unverified";
            }
            for (_, manifest) in &pending {
                if manifest
                    .entries
                    .iter()
                    .any(|entry| active.contains(&entry.thread_id))
                {
                    active.extend(manifest.entries.iter().map(|entry| entry.thread_id.clone()));
                }
            }
            if active.len() == before {
                break;
            }
        }
        pending.retain(|(_, manifest)| {
            !manifest
                .entries
                .iter()
                .any(|entry| active.contains(&entry.thread_id))
        });
        recover_pending(&home, &repair_root, pending)?;
        infos.retain(
            |info| match coordination::lineage_is_idle(&paths, info, &active) {
                Ok(true) => true,
                Ok(false) => {
                    report.skip(blocked_reason);
                    false
                }
                Err(error) => {
                    report.skip(error.reason());
                    false
                }
            },
        );
    } else {
        recover_pending(&home, &repair_root, pending)?;
    }

    planning::repair(&home, &repair_root, &infos, automatic, &paths, &mut report)?;
    Ok((
        HistoryRepairLock {
            _coordination: Some(coordination),
        },
        report,
    ))
}

fn ensure_repair_root(home: &Path) -> Result<PathBuf, HistoryRepairError> {
    let mut current = home.to_path_buf();
    for component in Path::new(REPAIR_DIRECTORY).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(HistoryRepairError::new("repair_directory_unsafe"));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(HistoryRepairError::new("repair_directory_unsafe"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|_| HistoryRepairError::new("repair_directory_unavailable"))?;
            }
            Err(_) => return Err(HistoryRepairError::new("repair_directory_unavailable")),
        }
    }
    let canonical = current
        .canonicalize()
        .map_err(|_| HistoryRepairError::new("repair_directory_unavailable"))?;
    if !canonical.starts_with(home) {
        return Err(HistoryRepairError::new("repair_directory_unsafe"));
    }
    Ok(canonical)
}

#[cfg(test)]
#[path = "history_repair/tests.rs"]
mod tests;
