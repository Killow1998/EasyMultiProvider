//! Database lookup, ancestor resolution and containment before opening rollouts.
use super::records::{locate, session_meta_history_mode, session_meta_id};
use super::source::{read_history_base, read_session_meta};
use super::{CodexHomeHistoryReader, MAX_LINEAGE_SEGMENTS};
use emp_history::HistoryError;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_SESSION_DIRECTORY_DEPTH: usize = 8;

pub(super) struct Location {
    pub(super) path: PathBuf,
    pub(super) mode: String,
    pub(super) source_model: Option<String>,
}

/// One rollout segment of a paginated lineage, replayed oldest first.
pub(super) struct LineageSegment {
    /// Rollout identifier owning the segment.
    pub(super) thread: String,
    /// Resolved rollout file and history mode.
    pub(super) location: Location,
    /// Records at or above this ordinal belong to the next-younger segment.
    pub(super) bound: Option<u64>,
    /// Frozen physical prefix (`history_base.end_byte_offset`); 0 disables
    /// the byte cutoff, keeping the ordinal bound as the only boundary.
    pub(super) byte_offset: u64,
}

impl CodexHomeHistoryReader {
    /// Resolve the lineage segments for a paginated rollout, child last.
    ///
    /// The child rollout is segment zero; each `history_base` pointer walks
    /// one ancestor older. Cycles and unbounded chains are rejected; a
    /// missing ancestor surfaces as `source_missing` because the visible
    /// history would be silently incomplete otherwise.
    pub(super) fn lineage_segments(
        &self,
        database: &Path,
        path: &Path,
        thread: &str,
    ) -> Result<Vec<LineageSegment>, HistoryError> {
        let location = locate(database, thread)?;
        let mut segments = vec![LineageSegment {
            thread: thread.to_owned(),
            location,
            bound: None,
            byte_offset: 0,
        }];
        let mut visited = vec![thread.to_owned()];
        let mut next_base = read_history_base(&self.contained_rollout_path(path)?)?;
        while let Some(base) = next_base {
            if visited.contains(&base.thread_id) {
                return Err(HistoryError::new("lineage_cycle"));
            }
            if segments.len() >= MAX_LINEAGE_SEGMENTS {
                return Err(HistoryError::new("lineage_depth_exceeded"));
            }
            let parent_id = base.thread_id;
            let bound = base.end_ordinal_exclusive;
            let byte_offset = base.end_byte_offset;
            visited.push(parent_id.clone());
            let parent_location = self.locate_parent(database, &parent_id)?;
            next_base = read_history_base(&parent_location.path)?;
            segments.push(LineageSegment {
                thread: parent_id,
                location: parent_location,
                bound: Some(bound),
                byte_offset,
            });
        }
        // Chain was collected child-first; replay order is oldest first.
        segments.reverse();
        Ok(segments)
    }

    /// Resolve a lineage ancestor either through the threads database or by
    /// scanning the sessions tree for the rollout file name.
    fn locate_parent(&self, database: &Path, rollout_id: &str) -> Result<Location, HistoryError> {
        let roots = self.canonical_session_roots()?;
        match locate(database, rollout_id) {
            Ok(location) => {
                if self.is_under_session_root(&location.path, &roots) {
                    return Ok(location);
                }
                return Err(HistoryError::new("rollout_outside_session_root"));
            }
            Err(error) if error.reason() != "thread_missing" => return Err(error),
            Err(_) => {}
        }
        let expected = [
            format!("{rollout_id}.jsonl"),
            format!("{rollout_id}.jsonl.zst"),
        ];
        let mut found = BTreeSet::new();
        for root in &roots {
            let mut stack = vec![(root.clone(), 0usize)];
            while let Some((directory, depth)) = stack.pop() {
                let metadata = match fs::symlink_metadata(&directory) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    continue;
                }
                let entries = match fs::read_dir(&directory) {
                    Ok(entries) => entries,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(file_type) => file_type,
                        Err(_) => continue,
                    };
                    if file_type.is_symlink() {
                        continue;
                    }
                    if file_type.is_dir() {
                        if depth < MAX_SESSION_DIRECTORY_DEPTH {
                            stack.push((path, depth + 1));
                        }
                        continue;
                    }
                    if file_type.is_file()
                        && path
                            .file_name()
                            .and_then(OsStr::to_str)
                            .is_some_and(|name| expected.iter().any(|candidate| candidate == name))
                        && let Ok(canonical) = path.canonicalize()
                        && canonical.starts_with(root)
                    {
                        found.insert(canonical);
                    }
                }
            }
        }
        match found.len() {
            0 => Err(HistoryError::new("source_missing")),
            1 => {
                let path = found.into_iter().next().expect("one parent file");
                let meta = read_session_meta(&path)?
                    .ok_or_else(|| HistoryError::new("session_meta_missing"))?;
                let id = session_meta_id(&meta)
                    .ok_or_else(|| HistoryError::new("session_meta_missing"))?;
                if id != rollout_id {
                    return Err(HistoryError::new("thread_mismatch"));
                }
                let mode =
                    session_meta_history_mode(&meta)?.unwrap_or_else(|| "paginated".to_owned());
                Ok(Location {
                    path,
                    mode,
                    source_model: None,
                })
            }
            _ => Err(HistoryError::new("lineage_source_ambiguous")),
        }
    }

    fn canonical_session_roots(&self) -> Result<Vec<PathBuf>, HistoryError> {
        let home = self
            .home
            .canonicalize()
            .map_err(|_| HistoryError::new("history_unavailable"))?;
        let mut roots = Vec::new();
        for name in ["sessions", "archived_sessions"] {
            let path = home.join(name);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                continue;
            }
            if let Ok(canonical) = path.canonicalize()
                && canonical.starts_with(&home)
            {
                roots.push(canonical);
            }
        }
        Ok(roots)
    }

    fn is_under_session_root(&self, path: &Path, roots: &[PathBuf]) -> bool {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(directory) => directory.join(path),
                Err(_) => return false,
            }
        };
        let Ok(canonical) = absolute.canonicalize() else {
            return false;
        };
        if !canonical.is_file() {
            return false;
        }
        roots.iter().any(|root| {
            if !canonical.starts_with(root) {
                return false;
            }
            // `absolute` may reach the root through a symlinked or otherwise
            // non-canonical home; strip the ancestor that resolves to the
            // root, then require every component below it to be a real entry.
            let Some(relative) = absolute.ancestors().skip(1).find_map(|ancestor| {
                (ancestor.canonicalize().ok()? == *root)
                    .then(|| absolute.strip_prefix(ancestor).ok())
                    .flatten()
            }) else {
                return false;
            };
            let mut current = root.clone();
            for component in relative.components() {
                let Component::Normal(name) = component else {
                    return false;
                };
                current.push(name);
                let Ok(metadata) = fs::symlink_metadata(&current) else {
                    return false;
                };
                if metadata.file_type().is_symlink() {
                    return false;
                }
            }
            true
        })
    }

    pub(super) fn latest_state_database(&self) -> Result<PathBuf, HistoryError> {
        fs::read_dir(&self.home)
            .map_err(|_| HistoryError::new("state_database_missing"))?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                // SQLite -wal/-shm sidecars share the `state_N` stem but are
                // not queryable databases; only the exact `state_N.sqlite`
                // file can answer the thread lookup.
                if path.extension().and_then(|extension| extension.to_str()) != Some("sqlite") {
                    return None;
                }
                let name = path.file_stem()?.to_str()?;
                let version = name.strip_prefix("state_")?.parse::<u64>().ok()?;
                path.is_file().then_some((version, path))
            })
            .max_by_key(|(version, _)| *version)
            .map(|(_, path)| path)
            .ok_or_else(|| HistoryError::new("state_database_missing"))
    }

    /// Canonical rollout path, provided it is a regular file inside the
    /// Codex home. Checked before any open so a database row cannot point a
    /// read at a FIFO or at a file outside the home.
    pub(super) fn contained_rollout_path(&self, path: &Path) -> Result<PathBuf, HistoryError> {
        let home = self
            .home
            .canonicalize()
            .map_err(|_| HistoryError::new("history_unavailable"))?;
        let path = path
            .canonicalize()
            .map_err(|_| HistoryError::new("source_missing"))?;
        if !path.starts_with(&home) {
            return Err(HistoryError::new("rollout_outside_codex_home"));
        }
        let metadata = fs::metadata(&path).map_err(|_| HistoryError::new("source_missing"))?;
        if !metadata.is_file() {
            return Err(HistoryError::new("source_missing"));
        }
        Ok(path)
    }
}
