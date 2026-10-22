//! Read-only discovery and validation of Codex history sources.
use super::HistoryRepairError;
use crate::history::{CodexHomeHistoryReader, uuid_shape};
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_REPAIR_THREADS: usize = 100_000;

#[derive(Clone)]
pub(super) struct ThreadInfo {
    pub(super) id: String,
    pub(super) path: PathBuf,
    pub(super) mode: Option<String>,
}

pub(super) fn canonical_home(home: &Path) -> Result<PathBuf, HistoryRepairError> {
    let metadata = fs::symlink_metadata(home)
        .map_err(|_| HistoryRepairError::new("codex_home_unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HistoryRepairError::new("codex_home_unsafe"));
    }
    home.canonicalize()
        .map_err(|_| HistoryRepairError::new("codex_home_unavailable"))
}

pub(super) fn has_rollouts(home: &Path) -> Result<bool, HistoryRepairError> {
    Ok(session_roots(home)?
        .iter()
        .any(|root| directory_has_rollouts(root)))
}

pub(super) fn threads(
    home: &Path,
    selected: Option<&str>,
) -> Result<Option<Vec<ThreadInfo>>, HistoryRepairError> {
    let database = match CodexHomeHistoryReader::new(home).latest_state_database() {
        Ok(path) => path,
        Err(error) if error.reason() == "state_database_missing" => {
            if selected.is_some() {
                return Err(HistoryRepairError::new("thread_not_found"));
            }
            if has_rollouts(home)? {
                return Err(HistoryRepairError::new(
                    "state_database_missing_for_existing_history",
                ));
            }
            return Ok(None);
        }
        Err(_) => return Err(HistoryRepairError::new("state_database_unavailable")),
    };
    let infos = read_thread_infos(home, &database, selected)?;
    if selected.is_some() && infos.is_empty() {
        return Err(HistoryRepairError::new("thread_not_found"));
    }
    Ok(Some(infos))
}

pub(super) fn session_roots(home: &Path) -> Result<Vec<PathBuf>, HistoryRepairError> {
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

fn read_thread_infos(
    home: &Path,
    database: &Path,
    selected: Option<&str>,
) -> Result<Vec<ThreadInfo>, HistoryRepairError> {
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
    let selection = if selected.is_some() {
        "WHERE id = ?1"
    } else {
        ""
    };
    let query = format!("SELECT id, rollout_path, {mode} FROM threads {selection} ORDER BY id");
    let mut statement = connection
        .prepare(&query)
        .map_err(|_| HistoryRepairError::new("threads_schema_unsupported"))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(selected), |row| {
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

pub(super) fn safe_rollout_path(
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
