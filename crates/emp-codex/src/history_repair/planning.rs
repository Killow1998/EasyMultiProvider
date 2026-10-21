//! Shared rewrite planning for explicit and idle-history migration.
use super::checkpoint::{has_emp1_tail_record, write_replacement_rollout};
use super::inventory::ThreadInfo;
use super::replay::context_for;
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
) -> Result<HistoryRepairReport, HistoryRepairError> {
    if infos.is_empty() {
        return Ok(HistoryRepairReport::default());
    }
    let affected = select_affected_rollouts(home, infos)?;
    if affected.is_empty() {
        return Ok(HistoryRepairReport::default());
    }
    let workspace = RepairWorkspace::begin(home, repair_root)?;
    let mut rewrites = Vec::new();
    let (mut converted, mut deferred) = (0usize, 0usize);
    for info in affected {
        match plan_rewrite(home, &workspace, &info) {
            Ok(Some((rewrite, changed))) => {
                converted = converted.saturating_add(changed);
                rewrites.push(rewrite);
            }
            Ok(None) => {}
            Err(_) if automatic => deferred += 1,
            Err(error) => return Err(error),
        }
    }
    let mut report = if rewrites.is_empty() {
        HistoryRepairReport::default()
    } else {
        workspace.apply_rewrites(rewrites, converted)?
    };
    report.threads_deferred = deferred;
    Ok(report)
}

fn plan_rewrite(
    home: &Path,
    workspace: &RepairWorkspace,
    info: &ThreadInfo,
) -> Result<Option<(PlannedRewrite, usize)>, HistoryRepairError> {
    let thread_id = info.id.clone();
    let lineage_directory = workspace
        .path()
        .join("workspace")
        .join(format!("lineage-{thread_id}"));
    let mut snapshot = ContextSnapshot::new(&lineage_directory)?;
    let rollout = context_for(home, &mut snapshot, info, None, &mut Vec::new())?;
    if snapshot.turns.has_open_turns()? {
        return Err(HistoryRepairError::new("unfinished_turn_in_history"));
    }
    if has_emp1_tail_record(&mut snapshot)? {
        return Err(HistoryRepairError::new(
            "unsupported_emp_checkpoint_location",
        ));
    }
    let replacement_path = workspace
        .path()
        .join("workspace")
        .join(format!("replacement-{thread_id}.spool"));
    let changed = write_replacement_rollout(&rollout, &mut snapshot, &replacement_path)?;
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
