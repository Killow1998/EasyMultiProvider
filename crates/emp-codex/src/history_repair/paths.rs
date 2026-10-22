//! One operation's rollout inventory, shared by writer guards and replay.
use super::HistoryRepairError;
use super::inventory::{ThreadInfo, safe_rollout_path, session_roots};
use crate::history::uuid_shape;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

type Index = BTreeMap<String, BTreeSet<PathBuf>>;

pub(super) struct RolloutPaths<'a> {
    home: &'a Path,
    roots: Vec<PathBuf>,
    discovered: OnceCell<Result<Index, HistoryRepairError>>,
}

impl<'a> RolloutPaths<'a> {
    pub(super) fn new(home: &'a Path) -> Result<Self, HistoryRepairError> {
        Ok(Self {
            home,
            roots: session_roots(home)?,
            discovered: OnceCell::new(),
        })
    }

    pub(super) fn resolve(&self, info: &ThreadInfo) -> Result<PathBuf, HistoryRepairError> {
        match safe_rollout_path(self.home, &info.path, &self.roots) {
            Ok(path) => Ok(path),
            Err(error) if error.reason() == "rollout_missing" => self.find(&info.id),
            Err(error) => Err(error),
        }
    }

    pub(super) fn find(&self, id: &str) -> Result<PathBuf, HistoryRepairError> {
        // Include misses and discovery failures: stale database paths must not
        // cause a complete directory walk for every thread or ancestor.
        let index = self
            .discovered
            .get_or_init(|| self.discover())
            .as_ref()
            .map_err(Clone::clone)?;
        let found = index
            .get(id)
            .ok_or_else(|| HistoryRepairError::new("lineage_source_missing"))?;
        if found.len() != 1 {
            return Err(HistoryRepairError::new("lineage_source_ambiguous"));
        }
        safe_rollout_path(
            self.home,
            found.first().expect("one lineage rollout"),
            &self.roots,
        )
    }

    fn discover(&self) -> Result<Index, HistoryRepairError> {
        let mut index = Index::new();
        for root in &self.roots {
            let mut stack = vec![(root.clone(), 0usize)];
            while let Some((directory, depth)) = stack.pop() {
                let entries = fs::read_dir(&directory)
                    .map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
                for entry in entries {
                    let entry = entry
                        .map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
                    let kind = entry
                        .file_type()
                        .map_err(|_| HistoryRepairError::new("session_directory_unavailable"))?;
                    if kind.is_symlink() {
                        return Err(HistoryRepairError::new("session_directory_unsafe"));
                    }
                    let path = entry.path();
                    if kind.is_dir() && depth < 8 {
                        stack.push((path, depth + 1));
                    } else if kind.is_file()
                        && let Some(id) = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .and_then(rollout_id)
                    {
                        let safe = safe_rollout_path(self.home, &path, &self.roots)?;
                        index.entry(id.to_owned()).or_default().insert(safe);
                    }
                }
            }
        }
        Ok(index)
    }
}

fn rollout_id(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    if uuid_shape(stem) {
        return Some(stem);
    }
    let (prefix, id) = stem.split_at_checked(stem.len().checked_sub(36)?)?;
    let timestamp = prefix.strip_prefix("rollout-")?.strip_suffix('-')?;
    (!timestamp.is_empty() && uuid_shape(id)).then_some(id)
}
