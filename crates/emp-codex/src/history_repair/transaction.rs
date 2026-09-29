use super::{HistoryRepairError, HistoryRepairReport, safe_rollout_path, session_roots};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub(super) struct PlannedRewrite {
    pub(super) thread_id: String,
    pub(super) path: PathBuf,
    pub(super) replacement_path: PathBuf,
    pub(super) before_sha256: String,
    pub(super) after_sha256: String,
    pub(super) before_bytes: u64,
    pub(super) after_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RepairManifest {
    pub(super) format_version: u32,
    pub(super) status: String,
    pub(super) entries: Vec<RepairManifestEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct RepairManifestEntry {
    pub(super) thread_id: String,
    pub(super) target: PathBuf,
    pub(super) backup: PathBuf,
    pub(super) stage: PathBuf,
    pub(super) before_sha256: String,
    pub(super) after_sha256: String,
    pub(super) before_bytes: u64,
    pub(super) after_bytes: u64,
}

struct StagedFileCleanup {
    paths: Vec<PathBuf>,
    retain: bool,
}

impl Drop for StagedFileCleanup {
    fn drop(&mut self) {
        if !self.retain {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub(super) struct RepairWorkspace {
    transaction: Option<tempfile::TempDir>,
    home: PathBuf,
    repair_root: PathBuf,
}

impl RepairWorkspace {
    pub(super) fn begin(home: &Path, repair_root: &Path) -> Result<Self, HistoryRepairError> {
        let transaction = tempfile::Builder::new()
            .prefix("repair-")
            .tempdir_in(repair_root)
            .map_err(|_| HistoryRepairError::new("repair_backup_unavailable"))?;
        let manifest = RepairManifest {
            format_version: 1,
            status: "preparing".to_owned(),
            entries: Vec::new(),
        };
        write_manifest(&transaction.path().join("manifest.json"), &manifest)?;
        sync_directory(repair_root)?;
        fs::create_dir(transaction.path().join("workspace"))
            .map_err(|_| HistoryRepairError::new("repair_workspace_unavailable"))?;
        sync_directory(transaction.path())?;
        Ok(Self {
            transaction: Some(transaction),
            home: home.to_path_buf(),
            repair_root: repair_root.to_path_buf(),
        })
    }

    pub(super) fn path(&self) -> &Path {
        self.transaction
            .as_ref()
            .expect("workspace transaction is present")
            .path()
    }

    pub(super) fn apply_rewrites(
        mut self,
        rewrites: Vec<PlannedRewrite>,
        converted: usize,
    ) -> Result<HistoryRepairReport, HistoryRepairError> {
        let transaction = self
            .transaction
            .as_ref()
            .ok_or_else(|| HistoryRepairError::new("repair_directory_unavailable"))?;
        let transaction_dir = transaction.path();
        let transaction_rel = transaction_dir
            .strip_prefix(&self.home)
            .map_err(|_| HistoryRepairError::new("repair_directory_unsafe"))?
            .to_path_buf();
        let transaction_id = transaction_dir
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| HistoryRepairError::new("repair_directory_unsafe"))?;
        let mut entries = Vec::with_capacity(rewrites.len());
        let mut stage_paths = Vec::with_capacity(rewrites.len());
        for rewrite in &rewrites {
            let target_rel = rewrite
                .path
                .strip_prefix(&self.home)
                .map_err(|_| HistoryRepairError::new("rollout_path_unsafe"))?
                .to_path_buf();
            let backup_rel = transaction_rel.join(format!("{}.original", rewrite.thread_id));
            let stage_name = format!(
                ".emp-history-repair-{transaction_id}-{}.stage",
                rewrite.thread_id
            );
            let stage_path = rewrite
                .path
                .parent()
                .ok_or_else(|| HistoryRepairError::new("rollout_path_unsafe"))?
                .join(stage_name);
            if fs::symlink_metadata(&stage_path).is_ok() {
                return Err(HistoryRepairError::new("repair_stage_conflict"));
            }
            let stage_rel = stage_path
                .strip_prefix(&self.home)
                .map_err(|_| HistoryRepairError::new("repair_stage_unsafe"))?
                .to_path_buf();
            stage_paths.push(stage_path);
            entries.push(RepairManifestEntry {
                thread_id: rewrite.thread_id.clone(),
                target: target_rel,
                backup: backup_rel,
                stage: stage_rel,
                before_sha256: rewrite.before_sha256.clone(),
                after_sha256: rewrite.after_sha256.clone(),
                before_bytes: rewrite.before_bytes,
                after_bytes: rewrite.after_bytes,
            });
        }
        let manifest_path = transaction_dir.join("manifest.json");
        let mut manifest = RepairManifest {
            format_version: 1,
            status: "preparing".to_owned(),
            entries,
        };
        write_manifest(&manifest_path, &manifest)?;
        sync_directory(&self.repair_root)?;

        let mut stage_cleanup = StagedFileCleanup {
            paths: Vec::with_capacity(stage_paths.len()),
            retain: false,
        };
        for ((rewrite, stage_path), entry) in
            rewrites.iter().zip(&stage_paths).zip(&manifest.entries)
        {
            let backup_path = self.home.join(&entry.backup);
            let permissions = fs::metadata(&rewrite.path)
                .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?
                .permissions();
            write_new_synced_from(
                &rewrite.path,
                &backup_path,
                None,
                entry.before_bytes,
                &entry.before_sha256,
                "repair_stale_snapshot",
            )?;
            write_new_synced_from(
                &rewrite.replacement_path,
                stage_path,
                Some(permissions),
                entry.after_bytes,
                &entry.after_sha256,
                "repair_stage_invalid",
            )?;
            stage_cleanup.paths.push(stage_path.clone());
            sync_directory(
                stage_path
                    .parent()
                    .ok_or_else(|| HistoryRepairError::new("repair_stage_unsafe"))?,
            )?;
        }
        let workspace_path = transaction_dir.join("workspace");
        fs::remove_dir_all(&workspace_path)
            .map_err(|_| HistoryRepairError::new("repair_workspace_cleanup_failed"))?;
        sync_directory(transaction_dir)?;
        manifest.status = "prepared".to_owned();
        write_manifest(&manifest_path, &manifest)?;
        let transaction = self
            .transaction
            .take()
            .ok_or_else(|| HistoryRepairError::new("repair_directory_unavailable"))?;
        let _transaction_dir = transaction.keep();
        stage_cleanup.retain = true;
        sync_directory(&self.repair_root)?;
        apply_manifest(&self.home, &manifest_path, manifest)?;
        Ok(HistoryRepairReport {
            threads_repaired: rewrites.len(),
            checkpoints_converted: converted,
        })
    }
}

#[cfg(test)]
pub(super) fn apply_rewrites(
    home: &Path,
    repair_root: &Path,
    rewrites: Vec<PlannedRewrite>,
    converted: usize,
) -> Result<HistoryRepairReport, HistoryRepairError> {
    RepairWorkspace::begin(home, repair_root)?.apply_rewrites(rewrites, converted)
}

#[cfg(test)]
fn write_new_synced(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<(), HistoryRepairError> {
    let parent = path
        .parent()
        .ok_or_else(|| HistoryRepairError::new("repair_file_unavailable"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    temporary
        .write_all(bytes)
        .map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    if let Some(permissions) = permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|_| HistoryRepairError::new("repair_stage_unavailable"))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    persist_temporary(temporary, path, false, "repair_file_unavailable")?;
    Ok(())
}

fn write_new_synced_from(
    source_path: &Path,
    path: &Path,
    permissions: Option<fs::Permissions>,
    expected_bytes: u64,
    expected_sha256: &str,
    mismatch_reason: &'static str,
) -> Result<(), HistoryRepairError> {
    let parent = path
        .parent()
        .ok_or_else(|| HistoryRepairError::new("repair_file_unavailable"))?;
    let mut source =
        File::open(source_path).map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    let (bytes, digest) = copy_and_hash(&mut source, temporary.as_file_mut())
        .map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    if bytes != expected_bytes || digest != expected_sha256 {
        return Err(HistoryRepairError::new(mismatch_reason));
    }
    if let Some(permissions) = permissions {
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|_| HistoryRepairError::new("repair_stage_unavailable"))?;
    }
    temporary
        .as_file()
        .sync_all()
        .map_err(|_| HistoryRepairError::new("repair_file_unavailable"))?;
    persist_temporary(temporary, path, false, "repair_file_unavailable")?;
    Ok(())
}

fn copy_and_hash(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<(u64, String)> {
    let mut digest = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        digest.update(&buffer[..count]);
        copied = copied
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("file length overflow"))?;
    }
    Ok((copied, format!("{:x}", digest.finalize())))
}

pub(super) fn file_fingerprint(path: &Path) -> io::Result<(u64, String)> {
    let mut source = File::open(path)?;
    copy_and_hash(&mut source, &mut io::sink())
}

fn persist_temporary(
    temporary: tempfile::NamedTempFile,
    path: &Path,
    replace_existing: bool,
    failure_reason: &'static str,
) -> Result<(), HistoryRepairError> {
    #[cfg(windows)]
    {
        let temporary_path = temporary.into_temp_path();
        move_file_write_through(&temporary_path, path, replace_existing)
            .map_err(|_| HistoryRepairError::new(failure_reason))
    }
    #[cfg(not(windows))]
    {
        if replace_existing {
            temporary
                .persist(path)
                .map(|_| ())
                .map_err(|_| HistoryRepairError::new(failure_reason))
        } else {
            temporary
                .persist_noclobber(path)
                .map(|_| ())
                .map_err(|_| HistoryRepairError::new(failure_reason))
        }
    }
}

pub(super) fn write_manifest(
    path: &Path,
    manifest: &RepairManifest,
) -> Result<(), HistoryRepairError> {
    let parent = path
        .parent()
        .ok_or_else(|| HistoryRepairError::new("manifest_path_unsafe"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|_| HistoryRepairError::new("manifest_unavailable"))?;
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|_| HistoryRepairError::new("manifest_serialize_failed"))?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|_| HistoryRepairError::new("manifest_unavailable"))?;
    persist_temporary(temporary, path, true, "manifest_unavailable")?;
    sync_directory(parent)
}

pub(super) fn pending_manifests(
    home: &Path,
    repair_root: &Path,
) -> Result<Vec<(PathBuf, RepairManifest)>, HistoryRepairError> {
    let mut pending = Vec::new();
    for entry in fs::read_dir(repair_root)
        .map_err(|_| HistoryRepairError::new("repair_directory_unavailable"))?
    {
        let entry = entry.map_err(|_| HistoryRepairError::new("repair_directory_unavailable"))?;
        let metadata = entry
            .file_type()
            .map_err(|_| HistoryRepairError::new("repair_directory_unavailable"))?;
        if metadata.is_symlink() || !metadata.is_dir() {
            return Err(HistoryRepairError::new("repair_manifest_unsafe"));
        }
        let manifest_path = entry.path().join("manifest.json");
        let metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(HistoryRepairError::new("manifest_unavailable")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(HistoryRepairError::new("repair_manifest_unsafe"));
        }
        let canonical = manifest_path
            .canonicalize()
            .map_err(|_| HistoryRepairError::new("manifest_unavailable"))?;
        if !canonical.starts_with(home) {
            return Err(HistoryRepairError::new("repair_manifest_unsafe"));
        }
        let manifest = serde_json::from_slice::<RepairManifest>(
            &fs::read(canonical).map_err(|_| HistoryRepairError::new("manifest_unavailable"))?,
        )
        .map_err(|_| HistoryRepairError::new("repair_manifest_invalid"))?;
        if manifest.format_version != 1
            || !matches!(
                manifest.status.as_str(),
                "preparing" | "prepared" | "committed" | "aborted"
            )
        {
            return Err(HistoryRepairError::new("repair_manifest_invalid"));
        }
        if matches!(manifest.status.as_str(), "preparing" | "prepared") {
            pending.push((manifest_path, manifest));
        }
    }
    Ok(pending)
}

pub(super) fn recover_pending(
    home: &Path,
    repair_root: &Path,
    pending: Vec<(PathBuf, RepairManifest)>,
) -> Result<(), HistoryRepairError> {
    let roots = session_roots(home)?;
    for (manifest_path, manifest) in pending {
        if manifest_path
            .parent()
            .is_none_or(|parent| !parent.starts_with(repair_root))
        {
            return Err(HistoryRepairError::new("repair_manifest_unsafe"));
        }
        if manifest.status == "preparing" {
            recover_preparing(home, &manifest_path, manifest)?;
            continue;
        }
        for entry in &manifest.entries {
            let target = resolve_manifest_rollout(home, &entry.target, &roots)?;
            let backup = resolve_home_file(home, &entry.backup)?;
            let (backup_bytes, backup_hash) = file_fingerprint(&backup)
                .map_err(|_| HistoryRepairError::new("repair_backup_missing"))?;
            if backup_bytes != entry.before_bytes || backup_hash != entry.before_sha256 {
                return Err(HistoryRepairError::new("repair_backup_invalid"));
            }
            let (current_bytes, current_hash) = file_fingerprint(&target)
                .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
            if current_hash == entry.after_sha256 {
                continue;
            }
            if current_bytes != entry.before_bytes || current_hash != entry.before_sha256 {
                return Err(HistoryRepairError::new("repair_stale_snapshot"));
            }
            let stage = resolve_home_file(home, &entry.stage)?;
            let (staged_bytes, staged_hash) = file_fingerprint(&stage)
                .map_err(|_| HistoryRepairError::new("repair_stage_missing"))?;
            if staged_bytes != entry.after_bytes || staged_hash != entry.after_sha256 {
                return Err(HistoryRepairError::new("repair_stage_invalid"));
            }
            replace_atomically(&stage, &target)?;
            sync_directory(
                target
                    .parent()
                    .ok_or_else(|| HistoryRepairError::new("rollout_path_unsafe"))?,
            )?;
        }
        let mut committed = manifest;
        committed.status = "committed".to_owned();
        write_manifest(&manifest_path, &committed)?;
    }
    Ok(())
}

pub(super) fn recover_preparing(
    home: &Path,
    manifest_path: &Path,
    mut manifest: RepairManifest,
) -> Result<(), HistoryRepairError> {
    let roots = session_roots(home)?;
    let transaction_dir = manifest_path
        .parent()
        .ok_or_else(|| HistoryRepairError::new("repair_manifest_unsafe"))?;
    let transaction_relative = transaction_dir
        .strip_prefix(home)
        .map_err(|_| HistoryRepairError::new("repair_manifest_unsafe"))?;
    let transaction_id = transaction_dir
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| value.starts_with("repair-"))
        .ok_or_else(|| HistoryRepairError::new("repair_manifest_unsafe"))?;
    for entry in &manifest.entries {
        let target = resolve_manifest_rollout(home, &entry.target, &roots)?;
        let current = file_fingerprint(&target)
            .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
        if current.0 != entry.before_bytes || current.1 != entry.before_sha256 {
            return Err(HistoryRepairError::new("repair_stale_snapshot"));
        }

        validate_preparing_artifact_paths(
            transaction_id,
            transaction_relative,
            &entry.target,
            entry,
        )?;

        if let Some(backup) = preparing_artifact_path(
            home,
            &entry.backup,
            "repair_manifest_unsafe",
            "repair_backup_unavailable",
        )? {
            let fingerprint = file_fingerprint(&backup)
                .map_err(|_| HistoryRepairError::new("repair_backup_unavailable"))?;
            if fingerprint.0 != entry.before_bytes || fingerprint.1 != entry.before_sha256 {
                fs::remove_file(&backup)
                    .map_err(|_| HistoryRepairError::new("repair_backup_cleanup_failed"))?;
                sync_directory(
                    backup
                        .parent()
                        .ok_or_else(|| HistoryRepairError::new("repair_manifest_unsafe"))?,
                )?;
            }
        }

        if let Some(stage) = preparing_artifact_path(
            home,
            &entry.stage,
            "repair_stage_unsafe",
            "repair_stage_unavailable",
        )? {
            fs::remove_file(&stage)
                .map_err(|_| HistoryRepairError::new("repair_stage_cleanup_failed"))?;
            sync_directory(
                stage
                    .parent()
                    .ok_or_else(|| HistoryRepairError::new("repair_stage_unsafe"))?,
            )?;
        }
    }
    remove_workspace_dir(transaction_dir)?;
    manifest.status = "aborted".to_owned();
    write_manifest(manifest_path, &manifest)
}

fn remove_workspace_dir(transaction_dir: &Path) -> Result<(), HistoryRepairError> {
    let workspace = transaction_dir.join("workspace");
    let metadata = match fs::symlink_metadata(&workspace) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(HistoryRepairError::new("repair_workspace_unavailable")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HistoryRepairError::new("repair_manifest_unsafe"));
    }
    let canonical = workspace
        .canonicalize()
        .map_err(|_| HistoryRepairError::new("repair_workspace_unavailable"))?;
    if canonical.parent() != transaction_dir.canonicalize().ok().as_deref() {
        return Err(HistoryRepairError::new("repair_manifest_unsafe"));
    }
    fs::remove_dir_all(&canonical)
        .map_err(|_| HistoryRepairError::new("repair_workspace_cleanup_failed"))?;
    sync_directory(transaction_dir)
}

fn validate_preparing_artifact_paths(
    transaction_id: &str,
    transaction_relative: &Path,
    target_relative: &Path,
    entry: &RepairManifestEntry,
) -> Result<(), HistoryRepairError> {
    let expected_backup = transaction_relative.join(format!("{}.original", entry.thread_id));
    if entry.backup != expected_backup {
        return Err(HistoryRepairError::new("repair_manifest_unsafe"));
    }

    let stage_parent_matches = entry.stage.parent() == target_relative.parent();
    let stage_name = entry.stage.file_name().and_then(|value| value.to_str());
    let current_stage_name = format!(
        ".emp-history-repair-{transaction_id}-{}.stage",
        entry.thread_id
    );
    let legacy_stage_name = transaction_id
        .strip_prefix("repair-")
        .map(|suffix| format!(".emp-history-repair-{suffix}.stage"));
    if !stage_parent_matches
        || !stage_name.is_some_and(|name| {
            name == current_stage_name || legacy_stage_name.as_deref() == Some(name)
        })
    {
        return Err(HistoryRepairError::new("repair_stage_unsafe"));
    }
    Ok(())
}

fn preparing_artifact_path(
    home: &Path,
    relative: &Path,
    unsafe_reason: &'static str,
    unavailable_reason: &'static str,
) -> Result<Option<PathBuf>, HistoryRepairError> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(HistoryRepairError::new(unsafe_reason));
    }
    let path = home.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| HistoryRepairError::new(unsafe_reason))?;
            let canonical_parent = parent
                .canonicalize()
                .map_err(|_| HistoryRepairError::new(unavailable_reason))?;
            if !canonical_parent.starts_with(home) {
                return Err(HistoryRepairError::new(unsafe_reason));
            }
            return Ok(None);
        }
        Err(_) => return Err(HistoryRepairError::new(unavailable_reason)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HistoryRepairError::new(unsafe_reason));
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| HistoryRepairError::new(unavailable_reason))?;
    if !canonical.starts_with(home) {
        return Err(HistoryRepairError::new(unsafe_reason));
    }
    Ok(Some(canonical))
}

fn apply_manifest(
    home: &Path,
    manifest_path: &Path,
    mut manifest: RepairManifest,
) -> Result<(), HistoryRepairError> {
    let roots = session_roots(home)?;
    for entry in &manifest.entries {
        let target = resolve_manifest_rollout(home, &entry.target, &roots)?;
        let backup = resolve_home_file(home, &entry.backup)?;
        let original = file_fingerprint(&backup)
            .map_err(|_| HistoryRepairError::new("repair_backup_missing"))?;
        if original.0 != entry.before_bytes || original.1 != entry.before_sha256 {
            return Err(HistoryRepairError::new("repair_backup_invalid"));
        }
        let current = file_fingerprint(&target)
            .map_err(|_| HistoryRepairError::new("rollout_unavailable"))?;
        if current.1 == entry.after_sha256 {
            continue;
        }
        if current.0 != entry.before_bytes || current.1 != entry.before_sha256 {
            return Err(HistoryRepairError::new("repair_stale_snapshot"));
        }
        let stage = resolve_home_file(home, &entry.stage)?;
        let staged = file_fingerprint(&stage)
            .map_err(|_| HistoryRepairError::new("repair_stage_missing"))?;
        if staged.0 != entry.after_bytes || staged.1 != entry.after_sha256 {
            return Err(HistoryRepairError::new("repair_stage_invalid"));
        }
        replace_atomically(&stage, &target)?;
        sync_directory(
            target
                .parent()
                .ok_or_else(|| HistoryRepairError::new("rollout_path_unsafe"))?,
        )?;
    }
    manifest.status = "committed".to_owned();
    write_manifest(manifest_path, &manifest)
}

fn resolve_manifest_rollout(
    home: &Path,
    relative: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, HistoryRepairError> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(HistoryRepairError::new("repair_manifest_unsafe"));
    }
    safe_rollout_path(home, &home.join(relative), roots)
}

fn resolve_home_file(home: &Path, relative: &Path) -> Result<PathBuf, HistoryRepairError> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(HistoryRepairError::new("repair_manifest_unsafe"));
    }
    let path = home.join(relative);
    let canonical = path
        .canonicalize()
        .map_err(|_| HistoryRepairError::new("repair_file_missing"))?;
    if !canonical.starts_with(home)
        || fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(HistoryRepairError::new("repair_manifest_unsafe"));
    }
    Ok(canonical)
}

fn replace_atomically(stage: &Path, target: &Path) -> Result<(), HistoryRepairError> {
    #[cfg(windows)]
    {
        move_file_write_through(stage, target, true)
            .map_err(|_| HistoryRepairError::new("rollout_publish_failed"))
    }
    #[cfg(not(windows))]
    {
        fs::rename(stage, target).map_err(|_| HistoryRepairError::new("rollout_publish_failed"))
    }
}

#[cfg(windows)]
fn move_file_write_through(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace_existing {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), HistoryRepairError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| HistoryRepairError::new("repair_sync_failed"))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), HistoryRepairError> {
    // Windows has no directory-fsync equivalent; every publication or
    // replacement instead uses MOVEFILE_WRITE_THROUGH.
    Ok(())
}

#[cfg(test)]
pub(super) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
#[path = "transaction_tests.rs"]
mod transaction_tests;
