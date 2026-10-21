//! Writer exclusion and active-fork protection for native history repair.
use super::HistoryRepairError;
use super::inventory::{ThreadInfo, find_rollout_for_id, session_roots};
use super::replay::load_rollout_header;
use crate::history::{MAX_LINEAGE_SEGMENTS as MAX_LINEAGE_DEPTH, uuid_shape};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::path::Path;

pub(super) const LOCK_DIRECTORY: &str = "thread-writer-locks";
pub(super) const COORDINATION_LOCK: &str = ".coordination.lock";

/// Holds Codex's writer coordination lock through configuration restoration.
#[derive(Debug)]
pub struct HistoryRepairLock {
    pub(super) _coordination: Option<File>,
}

pub(super) fn acquire_coordination_lock(
    home: &Path,
    nonblocking: bool,
) -> Result<File, HistoryRepairError> {
    let directory = home.join(LOCK_DIRECTORY);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(HistoryRepairError::new("writer_lock_directory_unsafe"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&directory)
                .map_err(|_| HistoryRepairError::new("writer_lock_directory_unavailable"))?;
        }
        Err(_) => return Err(HistoryRepairError::new("writer_lock_directory_unavailable")),
    }
    if directory.canonicalize().ok().as_deref() != Some(directory.as_path()) {
        return Err(HistoryRepairError::new("writer_lock_directory_unsafe"));
    }
    let path = directory.join(COORDINATION_LOCK);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(HistoryRepairError::new("writer_coordination_lock_unsafe"));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|_| HistoryRepairError::new("writer_coordination_lock_unavailable"))?;
    if nonblocking {
        file.try_lock().map_err(|error| {
            HistoryRepairError::new(match error {
                std::fs::TryLockError::WouldBlock => "active_codex_writer",
                std::fs::TryLockError::Error(_) => "writer_coordination_lock_unavailable",
            })
        })?;
    } else {
        file.lock()
            .map_err(|_| HistoryRepairError::new("writer_coordination_lock_unavailable"))?;
    }
    Ok(file)
}

/// Mirrors Codex 0.158.0's publication probe: while holding the shared
/// coordination lock, try each known writer lock and fail instead of waiting
/// on a per-thread lock. New writers cannot start until the caller finishes
/// migration and drops the coordination lock.
pub(super) fn probe_codex_writers(
    home: &Path,
    thread_ids: &BTreeSet<String>,
    all_writer_locks: bool,
) -> Result<BTreeSet<String>, HistoryRepairError> {
    let directory = home.join(LOCK_DIRECTORY);
    let mut candidates = thread_ids.clone();
    if all_writer_locks {
        for entry in fs::read_dir(&directory)
            .map_err(|_| HistoryRepairError::new("writer_lock_directory_unavailable"))?
        {
            let entry = entry.map_err(|_| HistoryRepairError::new("writer_lock_unavailable"))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == COORDINATION_LOCK {
                continue;
            }
            let metadata = entry
                .file_type()
                .map_err(|_| HistoryRepairError::new("writer_lock_unavailable"))?;
            if metadata.is_symlink() || !metadata.is_file() {
                return Err(HistoryRepairError::new("writer_lock_unsafe"));
            }
            let Some(thread_id) = name.strip_suffix(".lock") else {
                return Err(HistoryRepairError::new("writer_lock_unsafe"));
            };
            if !uuid_shape(thread_id) {
                return Err(HistoryRepairError::new("invalid_thread_id"));
            }
            candidates.insert(thread_id.to_owned());
        }
    }
    let mut active = BTreeSet::new();
    for thread_id in &candidates {
        if !uuid_shape(thread_id) {
            return Err(HistoryRepairError::new("invalid_thread_id"));
        }
        let path = directory.join(format!("{thread_id}.lock"));
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(HistoryRepairError::new("writer_lock_unavailable")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(HistoryRepairError::new("writer_lock_unsafe"));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|_| HistoryRepairError::new("writer_lock_unavailable"))?;
        match file.try_lock() {
            Ok(()) => drop(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                active.insert(thread_id.clone());
            }
            Err(std::fs::TryLockError::Error(_)) => {
                return Err(HistoryRepairError::new("writer_lock_unavailable"));
            }
        }
    }
    Ok(active)
}

/// A live descendant may still refer to its parent's byte offsets. Protect
/// those ancestors too; only the immutable session header is read here.
pub(super) fn protect_active_lineages(
    home: &Path,
    infos: &[ThreadInfo],
    active: &mut BTreeSet<String>,
) -> Result<(), HistoryRepairError> {
    let roots = session_roots(home)?;
    for id in active.clone() {
        let mut info = match infos.iter().find(|info| info.id == id) {
            Some(info) => info.clone(),
            None => ThreadInfo {
                path: find_rollout_for_id(home, &id, &roots)?,
                id,
                mode: None,
            },
        };
        let mut visited = BTreeSet::new();
        for depth in 0..MAX_LINEAGE_DEPTH {
            if !visited.insert(info.id.clone()) {
                return Err(HistoryRepairError::new("lineage_invalid"));
            }
            active.insert(info.id.clone());
            let header = load_rollout_header(home, &info)?;
            let Some(base) = header.history_base else {
                break;
            };
            if depth + 1 == MAX_LINEAGE_DEPTH {
                return Err(HistoryRepairError::new("lineage_invalid"));
            }
            info = ThreadInfo {
                path: find_rollout_for_id(home, &base.thread_id, &roots)?,
                id: base.thread_id,
                mode: None,
            };
        }
    }
    Ok(())
}

pub(super) fn lineage_is_idle(
    home: &Path,
    info: &ThreadInfo,
    active: &BTreeSet<String>,
) -> Result<bool, HistoryRepairError> {
    let mut info = info.clone();
    let mut visited = BTreeSet::new();
    let roots = session_roots(home)?;
    for _ in 0..MAX_LINEAGE_DEPTH {
        if active.contains(&info.id) {
            return Ok(false);
        }
        if !visited.insert(info.id.clone()) {
            return Err(HistoryRepairError::new("lineage_invalid"));
        }
        let header = load_rollout_header(home, &info)?;
        let Some(base) = header.history_base else {
            return Ok(true);
        };
        if active.contains(&base.thread_id) {
            return Ok(false);
        }
        let path = find_rollout_for_id(home, &base.thread_id, &roots)?;
        info = ThreadInfo {
            id: base.thread_id,
            path,
            mode: None,
        };
    }
    Err(HistoryRepairError::new("lineage_invalid"))
}
