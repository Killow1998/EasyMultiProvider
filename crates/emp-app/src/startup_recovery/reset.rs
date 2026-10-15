//! Stage old EMP files for rollback; retain them only when backup is selected.
use super::*;
use emp_state::filesystem::{
    atomic_write_private_state, create_private_directory, write_new_private_file,
};
use std::fs;

/// Caller holds state/service.lock throughout recovery and this transaction.
pub(super) fn rebuild(
    config: &Path,
    keep_backup: bool,
) -> Result<(Option<PathBuf>, bool), &'static str> {
    if external_key() {
        return Err("external_key");
    }
    rebuild_with(config, keep_backup, || Ok(()))
}

pub(super) fn rebuild_with(
    config: &Path,
    keep_backup: bool,
    before_install: impl FnOnce() -> Result<(), &'static str>,
) -> Result<(Option<PathBuf>, bool), &'static str> {
    let root = config.parent().ok_or("unsafe_path")?;
    let state = root.join("state");
    // The service lock validates the state ancestors. Also reject a config
    // symlink/reparse point; recovery must never follow it to unrelated data.
    if let Ok(metadata) = fs::symlink_metadata(config)
        && (!metadata.is_file() || linked(&metadata))
    {
        return Err("unsafe_path");
    }
    let marker = state.join("recovery-reset.json");
    let previous_marker = read_marker(&marker)?;
    let previous_backup = previous_marker
        .as_deref()
        .and_then(|bytes| pending_backup(config, bytes));
    let resuming = previous_backup.is_some();
    let nonce = crate::util::random_hex(12).map_err(|_| "backup_failed")?;
    let backup = previous_backup.unwrap_or_else(|| {
        root.join(format!(
            "recovery-{}-{}",
            if keep_backup { "backup" } else { "staging" },
            // A random directory is only used for a new transaction.
            nonce
        ))
    });
    create_private_directory(&backup).map_err(|_| "backup_failed")?;
    // Keep every prior attempt in the same transaction archive. A fresh attempt
    // can finish after interruption at any move without overwriting old files.
    let attempt = if resuming {
        backup.join(format!("attempt-{nonce}"))
    } else {
        backup.clone()
    };
    create_private_directory(&attempt).map_err(|_| "backup_failed")?;
    let old_state = attempt.join("state");
    let staged = attempt.join("new-state");
    create_private_directory(&old_state).map_err(|_| "backup_failed")?;
    create_private_directory(&staged).map_err(|_| "backup_failed")?;
    let mut fresh = emp_state::normalize_configuration(None).map_err(|_| "prepare_failed")?;
    fresh["auto_enable_on_start"] = Value::Bool(false);
    let fresh_config = attempt.join("new-config.json");
    write_new_private_file(
        &fresh_config,
        &serde_json::to_vec_pretty(&fresh).map_err(|_| "prepare_failed")?,
    )
    .map_err(|_| "prepare_failed")?;
    VaultStore::from_sources(None, &staged.join("master.key")).map_err(|_| "prepare_failed")?;
    let entries = fs::read_dir(&state)
        .map_err(|_| "backup_failed")?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "backup_failed")?;
    // A crash between file moves must reopen recovery, not silently start
    // from defaults beside a half-moved credential store.
    if let Some(bytes) = &previous_marker {
        write_new_private_file(&attempt.join("previous-reset.json"), bytes)
            .map_err(|_| "prepare_failed")?;
    }
    atomic_write_private_state(
        &marker,
        &serde_json::to_vec(&serde_json::json!({
            "config_path":config,"saved_files":backup,"keep_backup":keep_backup
        }))
        .map_err(|_| "prepare_failed")?,
    )
    .map_err(|_| "prepare_failed")?;
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    let result = (|| {
        let mut move_file = |from: PathBuf, to: PathBuf| -> Result<(), &'static str> {
            if to.try_exists().map_err(|_| "backup_failed")? {
                return Err("backup_failed");
            }
            fs::rename(&from, &to).map_err(|_| "backup_failed")?;
            moves.push((from, to));
            Ok(())
        };
        if config.try_exists().map_err(|_| "backup_failed")? {
            move_file(config.to_path_buf(), attempt.join("config.json"))?;
        }
        for entry in entries {
            // Keep the lock inode in place: another EMP must not obtain a new
            // lock file while this instance is rearranging its private state.
            if entry.file_name() != "service.lock" && entry.file_name() != "recovery-reset.json" {
                move_file(entry.path(), old_state.join(entry.file_name()))?;
            }
        }
        before_install()?;
        move_file(staged.join("master.key"), state.join("master.key"))?;
        move_file(fresh_config, config.to_path_buf())?;
        move_file(marker.clone(), attempt.join("completed.json"))?;
        Ok(())
    })();
    if let Err(error) = result {
        for (original, moved) in moves.iter().rev() {
            if fs::rename(moved, original).is_err() {
                return Err("rollback_failed");
            }
        }
        if let Some(bytes) = &previous_marker {
            atomic_write_private_state(&marker, bytes).map_err(|_| "rollback_failed")?;
        } else {
            fs::rename(&marker, attempt.join("rolled-back.json")).map_err(|_| "rollback_failed")?;
        }
        return Err(error);
    }
    if keep_backup {
        return Ok((Some(backup), false));
    }
    // Only the generated transaction directory is removed, after commit.
    // remove_dir_all does not follow symlinks contained in this directory.
    match fs::remove_dir_all(&backup) {
        Ok(()) => Ok((None, false)),
        Err(_) => Ok((Some(backup), true)),
    }
}

fn read_marker(marker: &Path) -> Result<Option<Vec<u8>>, &'static str> {
    match fs::symlink_metadata(marker) {
        Ok(metadata) if !metadata.is_file() || linked(&metadata) => Err("unsafe_path"),
        Ok(_) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            fs::File::open(marker)
                .map_err(|_| "prepare_failed")?
                .take(65537)
                .read_to_end(&mut bytes)
                .map_err(|_| "prepare_failed")?;
            if bytes.len() > 65536 {
                return Err("prepare_failed");
            }
            Ok(Some(bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("prepare_failed"),
    }
}

fn pending_backup(config: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let marker: Value = serde_json::from_slice(bytes).ok()?;
    if Path::new(marker["config_path"].as_str()?) != config {
        return None;
    }
    let path = PathBuf::from(marker["saved_files"].as_str()?);
    if path.parent()? != config.parent()? {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let suffix = name
        .strip_prefix("recovery-backup-")
        .or_else(|| name.strip_prefix("recovery-staging-"))?;
    if suffix.len() != 24 || !suffix.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let metadata = fs::symlink_metadata(&path).ok()?;
    (metadata.is_dir() && !linked(&metadata)).then_some(path)
}

fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
