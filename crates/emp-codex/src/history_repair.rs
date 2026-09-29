//! Reversible, append-only conversion of EMP portable checkpoints before
//! restoring Codex's native provider configuration.

use crate::history::{
    CodexHomeHistoryReader, HistoryBase, MAX_LINEAGE_SEGMENTS, MAX_ROLLOUT_LINE_BYTES,
    session_meta_history_base, session_meta_history_mode, session_meta_id, token, uuid_shape,
};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

const MAX_LINEAGE_DEPTH: usize = MAX_LINEAGE_SEGMENTS;
const MAX_REPAIR_THREADS: usize = 100_000;
const LOCK_DIRECTORY: &str = "thread-writer-locks";
const COORDINATION_LOCK: &str = ".coordination.lock";
const REPAIR_DIRECTORY: &str = "easy-multi-provider/integration/history-repair";

#[path = "history_repair/checkpoint.rs"]
mod checkpoint;
#[path = "history_repair/replay.rs"]
mod replay;
#[path = "history_repair/selection.rs"]
mod selection;
#[path = "history_repair/spool.rs"]
mod spool;
#[path = "history_repair/transaction.rs"]
mod transaction;
use checkpoint::{map_contains_emp1_encrypted_content, write_replacement_rollout};
use replay::context_for;
use selection::select_affected_rollouts;
use spool::{JsonlSpool, TurnStore};
use transaction::{
    PlannedRewrite, RepairWorkspace, file_fingerprint, pending_manifests, recover_pending,
};
#[cfg(test)]
use transaction::{RepairManifest, RepairManifestEntry, recover_preparing, sha256, write_manifest};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryRepairReport {
    pub threads_repaired: usize,
    pub checkpoints_converted: usize,
}

/// Keeps Codex's cross-process writer coordination lock held until the caller
/// has restored the native configuration. This closes the gap where a new
/// rollout could be written after repair but before configuration restore.
#[derive(Debug)]
pub struct HistoryRepairLock {
    _coordination: File,
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

#[derive(Clone)]
struct ThreadInfo {
    id: String,
    path: PathBuf,
    mode: Option<String>,
}

struct ContextSnapshot {
    directory: PathBuf,
    history: JsonlSpool,
    metadata: JsonlSpool,
    tail_records: JsonlSpool,
    latest_checkpoint: Option<Map<String, Value>>,
    latest_token_usage: Option<Value>,
    turns: TurnStore,
}

#[derive(Clone, Copy)]
struct HistoryBound {
    ordinal_exclusive: u64,
    byte_offset: u64,
}

impl ContextSnapshot {
    fn new(directory: &Path) -> Result<Self, HistoryRepairError> {
        fs::create_dir(directory)
            .map_err(|_| HistoryRepairError::new("repair_spool_unavailable"))?;
        Ok(Self {
            directory: directory.to_path_buf(),
            history: JsonlSpool::create(directory.join("history.jsonl"))?,
            metadata: JsonlSpool::create(directory.join("metadata.jsonl"))?,
            tail_records: JsonlSpool::create(directory.join("tail.jsonl"))?,
            latest_checkpoint: None,
            latest_token_usage: None,
            turns: TurnStore::open(&directory.join("turns.sqlite"))?,
        })
    }

    fn has_emp1_tail_record(&mut self) -> Result<bool, HistoryRepairError> {
        let mut tail = self.tail_records.reader()?;
        while let Some(record) = tail.next::<Map<String, Value>>()? {
            if map_contains_emp1_encrypted_content(&record) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Convert EMP-owned `emp1:` compaction response items in every Codex thread
/// and existing paginated descendant. The returned lock must stay alive until
/// the native configuration restore has completed.
pub fn repair_before_native_restore(
    codex_home: &Path,
) -> Result<(HistoryRepairLock, HistoryRepairReport), HistoryRepairError> {
    let home = canonical_home(codex_home)?;
    let coordination = acquire_coordination_lock(&home)?;
    let repair_root = ensure_repair_root(&home)?;

    let database = match CodexHomeHistoryReader::new(&home).latest_state_database() {
        Ok(path) => path,
        Err(error) if error.reason() == "state_database_missing" => {
            if session_roots(&home)?
                .iter()
                .any(|root| directory_has_rollouts(root))
            {
                return Err(HistoryRepairError::new(
                    "state_database_missing_for_existing_history",
                ));
            }
            return Ok((
                HistoryRepairLock {
                    _coordination: coordination,
                },
                empty_report(),
            ));
        }
        Err(_) => return Err(HistoryRepairError::new("state_database_unavailable")),
    };
    let infos = read_thread_infos(&home, &database)?;
    let pending = pending_manifests(&home, &repair_root)?;
    let mut lock_ids = infos
        .iter()
        .map(|row| row.id.clone())
        .collect::<BTreeSet<_>>();
    for (_, manifest) in &pending {
        lock_ids.extend(manifest.entries.iter().map(|entry| entry.thread_id.clone()));
    }
    probe_codex_writers(&home, &lock_ids)?;
    recover_pending(&home, &repair_root, pending)?;

    if infos.is_empty() {
        return Ok((
            HistoryRepairLock {
                _coordination: coordination,
            },
            empty_report(),
        ));
    }

    let affected = select_affected_rollouts(&home, &infos)?;
    if affected.is_empty() {
        return Ok((
            HistoryRepairLock {
                _coordination: coordination,
            },
            empty_report(),
        ));
    }

    let workspace = RepairWorkspace::begin(&home, &repair_root)?;
    let mut rewrites = Vec::new();
    let mut converted = 0usize;
    for info in affected {
        let thread_id = info.id.clone();
        let lineage_directory = workspace
            .path()
            .join("workspace")
            .join(format!("lineage-{thread_id}"));
        let mut snapshot = ContextSnapshot::new(&lineage_directory)?;
        let rollout = context_for(&home, &mut snapshot, &info, None, &mut Vec::new())?;
        if snapshot.turns.has_open_turns()? {
            return Err(HistoryRepairError::new("unfinished_turn_in_history"));
        }
        if snapshot.has_emp1_tail_record()? {
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
            continue;
        }
        converted = converted.saturating_add(changed);
        let (after_bytes, after_sha256) = file_fingerprint(&replacement_path)
            .map_err(|_| HistoryRepairError::new("repair_stage_invalid"))?;
        rewrites.push(PlannedRewrite {
            thread_id,
            path: rollout.path,
            replacement_path,
            before_sha256: rollout.original_sha256,
            after_sha256,
            before_bytes: rollout.original_bytes,
            after_bytes,
        });
    }

    let report = if rewrites.is_empty() {
        drop(workspace);
        empty_report()
    } else {
        workspace.apply_rewrites(rewrites, converted)?
    };
    Ok((
        HistoryRepairLock {
            _coordination: coordination,
        },
        report,
    ))
}

fn empty_report() -> HistoryRepairReport {
    HistoryRepairReport {
        threads_repaired: 0,
        checkpoints_converted: 0,
    }
}

fn canonical_home(home: &Path) -> Result<PathBuf, HistoryRepairError> {
    let metadata = fs::symlink_metadata(home)
        .map_err(|_| HistoryRepairError::new("codex_home_unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HistoryRepairError::new("codex_home_unsafe"));
    }
    home.canonicalize()
        .map_err(|_| HistoryRepairError::new("codex_home_unavailable"))
}

fn acquire_coordination_lock(home: &Path) -> Result<File, HistoryRepairError> {
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
    file.lock()
        .map_err(|_| HistoryRepairError::new("writer_coordination_lock_unavailable"))?;
    Ok(file)
}

/// Mirrors Codex 0.158.0's publication probe: while holding the shared
/// coordination lock, try each known writer lock and fail instead of waiting
/// on a per-thread lock. New writers cannot start until the caller restores
/// configuration and drops the coordination lock.
fn probe_codex_writers(
    home: &Path,
    thread_ids: &BTreeSet<String>,
) -> Result<(), HistoryRepairError> {
    let directory = home.join(LOCK_DIRECTORY);
    let mut candidates = thread_ids.clone();
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
                return Err(HistoryRepairError::new("active_codex_writer"));
            }
            Err(std::fs::TryLockError::Error(_)) => {
                return Err(HistoryRepairError::new("writer_lock_unavailable"));
            }
        }
    }
    Ok(())
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

fn session_roots(home: &Path) -> Result<Vec<PathBuf>, HistoryRepairError> {
    let mut roots = Vec::new();
    for name in ["sessions", "archived_sessions"] {
        let path = home.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(HistoryRepairError::new("session_directory_unavailable")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(HistoryRepairError::new("session_directory_unsafe"));
        }
        let canonical = path
            .canonicalize()
            .map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
        if !canonical.starts_with(home) {
            return Err(HistoryRepairError::new("session_directory_unsafe"));
        }
        roots.push(canonical);
    }
    Ok(roots)
}

fn directory_has_rollouts(root: &Path) -> bool {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() && depth < 8 {
                stack.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                let name = entry.file_name();
                if name.to_string_lossy().ends_with(".jsonl")
                    || name.to_string_lossy().ends_with(".jsonl.zst")
                {
                    return true;
                }
            }
        }
    }
    false
}

fn read_thread_infos(home: &Path, database: &Path) -> Result<Vec<ThreadInfo>, HistoryRepairError> {
    let metadata = fs::symlink_metadata(database)
        .map_err(|_| HistoryRepairError::new("state_database_unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HistoryRepairError::new("state_database_unsafe"));
    }
    let canonical_db = database
        .canonicalize()
        .map_err(|_| HistoryRepairError::new("state_database_unavailable"))?;
    if !canonical_db.starts_with(home) {
        return Err(HistoryRepairError::new("state_database_unsafe"));
    }
    let connection = Connection::open_with_flags(&canonical_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| HistoryRepairError::new("state_database_unavailable"))?;
    let columns = connection
        .prepare("PRAGMA table_info(threads)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<BTreeSet<_>, _>>()
        })
        .map_err(|_| HistoryRepairError::new("threads_schema_unsupported"))?;
    if !columns.contains("id") || !columns.contains("rollout_path") {
        return Err(HistoryRepairError::new("threads_schema_unsupported"));
    }
    let mode = if columns.contains("history_mode") {
        "history_mode"
    } else {
        "NULL"
    };
    let query = format!("SELECT id, rollout_path, {mode} FROM threads ORDER BY id");
    let mut statement = connection
        .prepare(&query)
        .map_err(|_| HistoryRepairError::new("threads_schema_unsupported"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|_| HistoryRepairError::new("state_database_unavailable"))?;
    let mut infos = Vec::new();
    for row in rows {
        let (id, path, mode) =
            row.map_err(|_| HistoryRepairError::new("state_database_unavailable"))?;
        if !uuid_shape(&id) || path.trim().is_empty() {
            return Err(HistoryRepairError::new("invalid_thread_record"));
        }
        if mode
            .as_deref()
            .is_some_and(|value| !matches!(value, "legacy" | "paginated"))
        {
            return Err(HistoryRepairError::new("invalid_history_mode"));
        }
        if infos.len() >= MAX_REPAIR_THREADS {
            return Err(HistoryRepairError::new("repair_thread_limit_exceeded"));
        }
        let path = PathBuf::from(path.trim());
        let path = if path.is_absolute() {
            path
        } else {
            canonical_db.parent().unwrap_or(home).join(path)
        };
        infos.push(ThreadInfo { id, path, mode });
    }
    Ok(infos)
}

fn safe_rollout_path(
    home: &Path,
    requested: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, HistoryRepairError> {
    let metadata =
        fs::symlink_metadata(requested).map_err(|_| HistoryRepairError::new("rollout_missing"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HistoryRepairError::new("rollout_path_unsafe"));
    }
    let canonical = requested
        .canonicalize()
        .map_err(|_| HistoryRepairError::new("rollout_missing"))?;
    if !canonical.starts_with(home) || !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(HistoryRepairError::new("rollout_path_unsafe"));
    }
    let root = roots
        .iter()
        .find(|root| canonical.starts_with(root))
        .ok_or_else(|| HistoryRepairError::new("rollout_path_unsafe"))?;
    let relative = canonical
        .strip_prefix(root)
        .map_err(|_| HistoryRepairError::new("rollout_path_unsafe"))?;
    let mut current = root.clone();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(HistoryRepairError::new("rollout_path_unsafe"));
        };
        current.push(name);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|_| HistoryRepairError::new("rollout_missing"))?;
        if metadata.file_type().is_symlink() {
            return Err(HistoryRepairError::new("rollout_path_unsafe"));
        }
    }
    Ok(canonical)
}

fn find_rollout_for_id(
    home: &Path,
    thread_id: &str,
    roots: &[PathBuf],
) -> Result<PathBuf, HistoryRepairError> {
    let mut found = BTreeSet::new();
    for root in roots {
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((directory, depth)) = stack.pop() {
            let entries = fs::read_dir(&directory)
                .map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
            for entry in entries {
                let entry =
                    entry.map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
                let path = entry.path();
                let kind = entry
                    .file_type()
                    .map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
                if kind.is_symlink() {
                    return Err(HistoryRepairError::new("session_directory_unsafe"));
                }
                if kind.is_dir() && depth < 8 {
                    stack.push((path, depth + 1));
                } else if kind.is_file()
                    && path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .is_some_and(|name| rollout_filename_matches_thread(name, thread_id))
                {
                    found.insert(safe_rollout_path(home, &path, roots)?);
                }
            }
        }
    }
    match found.len() {
        0 => Err(HistoryRepairError::new("lineage_source_missing")),
        1 => Ok(found.into_iter().next().expect("one lineage rollout")),
        _ => Err(HistoryRepairError::new("lineage_source_ambiguous")),
    }
}

fn rollout_filename_matches_thread(name: &str, thread_id: &str) -> bool {
    [".jsonl.zst", ".jsonl"].iter().any(|extension| {
        let Some(stem) = name.strip_suffix(extension) else {
            return false;
        };
        if stem == thread_id {
            return true;
        }
        stem.strip_suffix(thread_id)
            .and_then(|prefix| prefix.strip_prefix("rollout-"))
            .and_then(|timestamp| timestamp.strip_suffix('-'))
            .is_some_and(|timestamp| !timestamp.is_empty())
    })
}

fn resolve_rollout_path(
    home: &Path,
    info: &ThreadInfo,
    roots: &[PathBuf],
) -> Result<PathBuf, HistoryRepairError> {
    match safe_rollout_path(home, &info.path, roots) {
        Ok(path) => Ok(path),
        Err(error) if error.reason() == "rollout_missing" => {
            find_rollout_for_id(home, &info.id, roots)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
#[path = "history_repair/tests.rs"]
mod tests;
