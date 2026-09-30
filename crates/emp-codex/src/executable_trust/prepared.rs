//! An executable plus ownership of any private, verified temporary snapshot.
use super::{TrustFailure, validate_executable};
use std::path::{Path, PathBuf};

/// Keep this owner alive until the child has exited. A macOS desktop engine
/// beneath shared Applications may need a temporary signed bundle snapshot;
/// that snapshot is never a cache and is removed when this owner is dropped.
#[derive(Debug)]
pub struct PreparedExecutable {
    path: PathBuf,
    selection: PathBuf,
    #[allow(dead_code)] // Owned for its cleanup, not read after preparation.
    snapshot: Option<tempfile::TempDir>,
}

impl PreparedExecutable {
    pub(crate) fn prepare(path: &Path) -> Result<Self, TrustFailure> {
        if !path.is_absolute() {
            return Err(TrustFailure::NotTrusted);
        }
        let canonical = path.canonicalize().map_err(|_| TrustFailure::Unavailable)?;
        match validate_executable(&canonical) {
            Ok(()) => Ok(Self {
                path: canonical,
                selection: path.to_owned(),
                snapshot: None,
            }),
            #[cfg(target_os = "macos")]
            Err(TrustFailure::Writable) => {
                let (directory, executable) = super::macos::prepare_snapshot(&canonical)?;
                Ok(Self::snapshot(directory, executable))
            }
            Err(error) => Err(error),
        }
    }

    /// The only path to execute. The snapshot owner must outlive the child.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A selector for quota's independent resolver, preserving npm's original
    /// symlink location for sibling Node discovery. For snapshots this is
    /// always the private destination, never the original shared App path.
    pub fn quota_selector(&self) -> &Path {
        &self.selection
    }

    #[cfg(any(target_os = "macos", all(test, unix)))]
    pub(super) fn snapshot(directory: tempfile::TempDir, executable: PathBuf) -> Self {
        Self {
            selection: executable.clone(),
            path: executable,
            snapshot: Some(directory),
        }
    }
}
