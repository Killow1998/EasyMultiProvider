//! Shared rewrite planning for explicit and idle-history migration.
use super::checkpoint::{has_emp1_tail_record, write_replacement_rollout};
use super::inventory::ThreadInfo;
use super::paths::RolloutPaths;
use super::replay::replay_closed_lineage;
use super::selection::select_affected_rollouts;
use super::spool::ContextSnapshot;
use super::transaction::{PlannedRewrite, RepairWorkspace, file_fingerprint};
use super::{HistoryRepairError, HistoryRepairReport};
use std::fs;
use std::path::Path;

pub(super) fn repair(
    home: &Path,
    repair_root: &Path,
    infos: &[ThreadInfo],
    automatic: bool,
    paths: &RolloutPaths,
    report: &mut HistoryRepairReport,
) -> Result<(), HistoryRepairError> {
    let affected = select_affected_rollouts(paths, infos)?;
    if affected.is_empty() {
        return Ok(());
    }
    let workspace = RepairWorkspace::begin(home, repair_root)?;
    let mut rewrites = Vec::new();
    let mut converted = 0usize;
    for affected in affected {
        match plan_rewrite(paths, &workspace.path().join("workspace"), affected.info) {
            Ok(Some((rewrite, changed))) => {
                converted = converted.saturating_add(changed);
                rewrites.push(rewrite);
            }
            Ok(None) => {}
            Err(error) if automatic => {
                if affected.confirmed {
                    report.defer(error.reason());
                } else {
                    report.skip(error.reason());
                }
            }
            Err(error) => return Err(error),
        }
    }
    if !rewrites.is_empty() {
        let applied = workspace.apply_rewrites(rewrites, converted)?;
        report.threads_repaired += applied.threads_repaired;
        report.checkpoints_converted += applied.checkpoints_converted;
    }
    Ok(())
}

pub(super) fn plan_rewrite(
    paths: &RolloutPaths,
    workspace: &Path,
    info: &ThreadInfo,
) -> Result<Option<(PlannedRewrite, usize)>, HistoryRepairError> {
    let thread_id = info.id.clone();
    let lineage_directory = workspace.join(format!("lineage-{thread_id}"));
    let mut snapshot = ContextSnapshot::new(&lineage_directory)?;
    let rollout = replay_closed_lineage(paths, &mut snapshot, info, None, &mut Vec::new())?;
    if has_emp1_tail_record(paths, &mut snapshot)? {
        return Err(HistoryRepairError::new(
            "unsupported_emp_checkpoint_location",
        ));
    }
    let replacement_path = workspace.join(format!("replacement-{thread_id}.spool"));
    let changed = write_replacement_rollout(paths, &rollout, &mut snapshot, &replacement_path)?;
    drop(snapshot);
    fs::remove_dir_all(&lineage_directory)
        .map_err(|_| HistoryRepairError::new("repair_spool_cleanup_failed"))?;
    if changed == 0 {
        return Ok(None);
    }
    let (after_bytes, after_sha256) = file_fingerprint(&replacement_path)
        .map_err(|_| HistoryRepairError::new("repair_stage_invalid"))?;
    Ok(Some((
        PlannedRewrite {
            thread_id,
            path: rollout.path,
            replacement_path,
            before_sha256: rollout.original_sha256,
            after_sha256,
            before_bytes: rollout.original_bytes,
            after_bytes,
        },
        changed,
    )))
}
