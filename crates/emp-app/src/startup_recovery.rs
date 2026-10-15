//! Startup recovery is available without decrypting the normal application state.
mod reset;
mod server;
#[cfg(test)]
mod tests;

use crate::error::AppError;
use emp_state::{FilesystemError, VaultStore};
use serde_json::Value;
pub(crate) use server::run;
use std::path::{Path, PathBuf};

pub(crate) fn external_key() -> bool {
    [
        emp_state::filesystem::MASTER_KEY_ENV,
        emp_state::filesystem::MASTER_KEY_FILE_ENV,
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|v| !v.to_string_lossy().trim().is_empty()))
}

fn credential_paths(config: &Value) -> Vec<PathBuf> {
    [("providers", "api_key_file"), ("accounts", "auth_file")]
        .into_iter()
        .flat_map(|(group, field)| {
            config[group]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |item| {
                    item[field]
                        .as_str()
                        .filter(|path| !path.is_empty())
                        .map(PathBuf::from)
                })
        })
        .collect()
}

pub(crate) fn open_vault(config: &Value, state_root: &Path) -> Result<VaultStore, AppError> {
    open_vault_with(config, state_root, external_key())
}

fn open_vault_with(
    config: &Value,
    state_root: &Path,
    external: bool,
) -> Result<VaultStore, AppError> {
    let paths = credential_paths(config);
    let key = state_root.join("master.key");
    // Never create a replacement key silently when a saved credential still
    // refers to the lost key. The recovery page must explain the decision.
    let inline_key = external
        && std::env::var_os(emp_state::filesystem::MASTER_KEY_ENV)
            .is_some_and(|v| !v.to_string_lossy().trim().is_empty());
    let source = if external {
        std::env::var(emp_state::filesystem::MASTER_KEY_FILE_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| PathBuf::from(v.trim()))
            .unwrap_or_else(|| key.clone())
    } else {
        key.clone()
    };
    if !inline_key
        && !paths.is_empty()
        && !emp_state::config::resolve_user_path(&source).try_exists()?
    {
        return Err(AppError::CredentialKeyMissing);
    }
    let vault = if external {
        VaultStore::from_environment(&key)?
    } else {
        VaultStore::from_sources(None, &key)?
    };
    if !paths.is_empty()
        && paths.iter().all(|path| {
            matches!(
                vault.read_encrypted_bytes(path),
                Err(FilesystemError::CredentialDecryptFailed)
            )
        })
    {
        return Err(FilesystemError::CredentialDecryptFailed.into());
    }
    Ok(vault)
}

pub(crate) fn recoverable(error: &AppError) -> bool {
    matches!(
        error,
        AppError::Config(_)
            | AppError::CredentialKeyMissing
            | AppError::RecoveryIncomplete
            | AppError::Filesystem(_)
    )
}

fn issue(error: &AppError) -> &'static str {
    match error {
        AppError::Config(_) => "config_invalid",
        AppError::CredentialKeyMissing => "key_missing",
        AppError::RecoveryIncomplete => "reset_incomplete",
        AppError::Filesystem(
            FilesystemError::KeyFileNotPrivate | FilesystemError::KeyFileNotOwned,
        ) => "key_permissions",
        AppError::Filesystem(FilesystemError::CredentialDecryptFailed) => "key_mismatch",
        AppError::Filesystem(
            FilesystemError::KeyFileInvalid | FilesystemError::EnvironmentKeyInvalid,
        ) => "key_invalid",
        _ => "state_unavailable",
    }
}

pub(crate) fn check_pending(config: &Path) -> Result<(), AppError> {
    if config
        .parent()
        .unwrap_or(Path::new("."))
        .join("state/recovery-reset.json")
        .try_exists()?
    {
        return Err(AppError::RecoveryIncomplete);
    }
    Ok(())
}
