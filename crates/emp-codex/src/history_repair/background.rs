use super::coordination::{
    COORDINATION_LOCK, LOCK_DIRECTORY, acquire_coordination_lock, lineage_is_idle,
    probe_codex_writers, protect_active_repairs,
};
use super::inventory::{self, ThreadInfo};
use super::paths::RolloutPaths;
use super::planning::plan_rewrite;
use super::selection::select_affected_rollouts;
use super::transaction::{PlannedRewrite, RepairWorkspace, pending_manifests, recover_pending};
use super::{HistoryRepairError, HistoryRepairReport, ensure_repair_root};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;
use std::vec::IntoIter;

/// One idle rollout per step; unchanged files are skipped on later sweeps.
pub struct IdleHistoryRepair {
    home: PathBuf,
    paths: RolloutPaths,
    pending: IntoIter<ThreadInfo>,
    inspected: BTreeMap<PathBuf, (u64, SystemTime)>,
}

pub(super) struct StagedRepair {
    _directory: tempfile::TempDir,
    rewrite: PlannedRewrite,
    converted: usize,
}

impl IdleHistoryRepair {
    pub fn new(home: &Path, shutdown: Arc<AtomicBool>) -> Result<Self, HistoryRepairError> {
        let home = inventory::canonical_home(home)?;
        if !home.join(LOCK_DIRECTORY).join(COORDINATION_LOCK).is_file() {
            return Err(HistoryRepairError::new("writer_coordination_unavailable"));
        }
        let mut paths = RolloutPaths::new(&home)?;
        paths.shutdown = Some(shutdown);
        paths.check_cancelled()?;
        let pending = inventory::threads(&home, None)?
            .unwrap_or_default()
            .into_iter();
        Ok(Self {
            home,
            paths,
            pending,
            inspected: BTreeMap::new(),
        })
    }

    pub fn refresh(&mut self) -> Result<(), HistoryRepairError> {
        self.paths.check_cancelled()?;
        let mut paths = RolloutPaths::new(&self.home)?;
        paths.shutdown = self.paths.shutdown.clone();
        self.pending = inventory::threads(&self.home, None)?
            .unwrap_or_default()
            .into_iter();
        self.paths = paths;
        Ok(())
    }

    pub fn step(&mut self) -> Result<Option<HistoryRepairReport>, HistoryRepairError> {
        loop {
            let Some(info) = self.pending.next() else {
                return Ok(None);
            };
            self.paths.check_cancelled()?;
            let mut report = HistoryRepairReport::default();
            let path = match self.paths.resolve(&info) {
                Ok(path) => path,
                Err(error) => {
                    report.skip(error.reason());
                    return Ok(Some(report));
                }
            };
            let metadata =
                fs::metadata(&path).map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
            let stamp = (
                metadata.len(),
                metadata
                    .modified()
                    .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?,
            );
            if self.inspected.get(&path) == Some(&stamp) {
                continue;
            }
            if !probe_codex_writers(&self.home, &BTreeSet::from([info.id.clone()]), false)?
                .is_empty()
            {
                report.skip("active_codex_writer");
                return Ok(Some(report));
            }
            let affected = select_affected_rollouts(&self.paths, std::slice::from_ref(&info))?;
            let Some(affected) = affected.first() else {
                self.inspected.insert(path, stamp);
                return Ok(Some(report));
            };
            let staged = match self.stage(&info) {
                Ok(Some(staged)) => staged,
                Ok(None) => {
                    self.inspected.insert(path, stamp);
                    return Ok(Some(report));
                }
                Err(error) if error.reason() == "history_repair_cancelled" => return Err(error),
                Err(error) => {
                    if affected.confirmed {
                        report.defer(error.reason());
                    } else {
                        report.skip(error.reason());
                    }
                    return Ok(Some(report));
                }
            };
            return self.commit(staged).map(Some);
        }
    }

    pub(super) fn stage(
        &self,
        info: &ThreadInfo,
    ) -> Result<Option<StagedRepair>, HistoryRepairError> {
        let repair_root = ensure_repair_root(&self.home)?;
        // No manifest until commit: restore must not recover a live scanner's spool.
        let directory = tempfile::Builder::new()
            .prefix("scan-")
            .tempdir_in(repair_root)
            .map_err(|_| HistoryRepairError::new("repair_workspace_unavailable"))?;
        let Some((rewrite, converted)) = plan_rewrite(&self.paths, directory.path(), info)? else {
            return Ok(None);
        };
        Ok(Some(StagedRepair {
            _directory: directory,
            rewrite,
            converted,
        }))
    }

    pub(super) fn commit(
        &self,
        staged: StagedRepair,
    ) -> Result<HistoryRepairReport, HistoryRepairError> {
        self.paths.check_cancelled()?;
        let _coordination = acquire_coordination_lock(&self.home, true)?;
        let infos = inventory::threads(&self.home, None)?.unwrap_or_default();
        let repair_root = ensure_repair_root(&self.home)?;
        let mut pending = pending_manifests(&self.home, &repair_root)?;
        let mut lock_ids = infos
            .iter()
            .map(|info| info.id.clone())
            .collect::<BTreeSet<_>>();
        lock_ids.extend(pending.iter().flat_map(|(_, manifest)| {
            manifest.entries.iter().map(|entry| entry.thread_id.clone())
        }));
        let mut active = probe_codex_writers(&self.home, &lock_ids, true)?;
        let paths = RolloutPaths::new(&self.home)?;
        let reason = protect_active_repairs(&paths, &infos, &lock_ids, &mut pending, &mut active);
        let info = infos
            .iter()
            .find(|info| info.id == staged.rewrite.thread_id)
            .ok_or_else(|| HistoryRepairError::new("thread_not_found"))?;
        if !lineage_is_idle(&paths, info, &active)? {
            let mut report = HistoryRepairReport::default();
            report.skip(reason);
            return Ok(report);
        }
        if paths.resolve(info)? != staged.rewrite.path {
            return Err(HistoryRepairError::new("repair_stale_snapshot"));
        }
        recover_pending(&self.home, &repair_root, pending)?;
        self.paths.check_cancelled()?;
        RepairWorkspace::begin(&self.home, &repair_root)?
            .apply_rewrites(vec![staged.rewrite], staged.converted)
    }
}
