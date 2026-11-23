//! Preserve the `easy-multi-provider-v1` magic and Fernet tokens.
//! Each runtime supplies its VaultStore; key paths are not process-global.

use crate::{FernetKey, VaultError, decode_vault, encode_vault};
use emp_files::filesystem::{
    atomic_write, ensure_managed_write_path, metadata_is_link_or_reparse, set_private_directory,
    set_private_file_mode, validate_private_key_file,
};
use emp_files::{FilesystemError, MAX_ENCRYPTED_FILE_BYTES, read_to_limit};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf, absolute};
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as StdOpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as StdOpenOptionsExt;

pub const MASTER_KEY_ENV: &str = "EASY_MULTI_PROVIDER_MASTER_KEY";
pub const MASTER_KEY_FILE_ENV: &str = "EASY_MULTI_PROVIDER_MASTER_KEY_FILE";

impl From<VaultError> for FilesystemError {
    fn from(error: VaultError) -> Self {
        match error {
            VaultError::NotVaultFormat => Self::UnsupportedCredentialFormat,
            VaultError::DecryptFailed => Self::CredentialDecryptFailed,
        }
    }
}

pub struct VaultStore {
    key: FernetKey,
    key_path: Option<PathBuf>,
}

impl VaultStore {
    /// Load the process environment once, using the supplied default path when
    /// no explicit key-file environment variable is set.
    pub fn from_environment(default_key_path: &Path) -> Result<Self, FilesystemError> {
        let environment_key = match env::var(MASTER_KEY_ENV) {
            Ok(value) => Some(Zeroizing::new(value)),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(FilesystemError::EnvironmentKeyInvalid);
            }
        };
        if let Some(raw) = environment_key
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            return Self::from_sources(Some(raw), default_key_path);
        }
        let configured_path = match env::var(MASTER_KEY_FILE_ENV) {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
            Ok(_) | Err(env::VarError::NotPresent) => default_key_path.to_path_buf(),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(FilesystemError::KeyDirectoryUnavailable);
            }
        };
        Self::from_sources(None, &configured_path)
    }

    pub fn from_sources(
        environment_key: Option<&str>,
        key_path: &Path,
    ) -> Result<Self, FilesystemError> {
        if let Some(raw) = environment_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let key =
                FernetKey::from_encoded(raw).map_err(|_| FilesystemError::EnvironmentKeyInvalid)?;
            return Ok(Self {
                key,
                key_path: None,
            });
        }

        let key_path = safe_key_path(key_path)?;
        let key = match read_key_file(&key_path) {
            Ok(key) => key,
            Err(FilesystemError::KeyFileUnavailable) => {
                create_key_file(&key_path)?;
                read_key_file(&key_path)?
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            key,
            key_path: Some(key_path),
        })
    }

    /// Return the durable key path, or None when the environment owns it.
    pub fn ensure_master_key(&self) -> Option<&Path> {
        self.key_path.as_deref()
    }

    pub fn write_encrypted_bytes(&self, path: &Path, value: &[u8]) -> Result<(), FilesystemError> {
        let ciphertext = encode_vault(&self.key, value);
        atomic_write(path, &ciphertext, 0o600, true)
            .map_err(|_| FilesystemError::CredentialFileUnavailable)
    }

    pub fn read_encrypted_bytes(&self, path: &Path) -> Result<Zeroizing<Vec<u8>>, FilesystemError> {
        ensure_managed_write_path(path).map_err(|_| FilesystemError::CredentialFileUnavailable)?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        #[cfg(windows)]
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let mut file = options
            .open(path)
            .map_err(|_| FilesystemError::CredentialFileUnavailable)?;
        let metadata = file
            .metadata()
            .map_err(|_| FilesystemError::CredentialFileUnavailable)?;
        if !metadata.is_file() || metadata_is_link_or_reparse(&metadata) {
            return Err(FilesystemError::CredentialFileUnavailable);
        }
        if metadata.len() > MAX_ENCRYPTED_FILE_BYTES as u64 {
            return Err(FilesystemError::CredentialFileTooLarge);
        }
        let raw = read_to_limit(&mut file, MAX_ENCRYPTED_FILE_BYTES).map_err(|error| {
            if error.kind() == std::io::ErrorKind::FileTooLarge {
                FilesystemError::CredentialFileTooLarge
            } else {
                FilesystemError::CredentialFileUnavailable
            }
        })?;
        decode_vault(&self.key, &raw).map_err(Into::into)
    }

    pub fn write_encrypted_json(&self, path: &Path, value: &Value) -> Result<(), FilesystemError> {
        let plaintext =
            Zeroizing::new(serde_json::to_vec(value).map_err(|_| FilesystemError::InvalidJson)?);
        self.write_encrypted_bytes(path, &plaintext)
    }

    pub fn read_encrypted_json(&self, path: &Path) -> Result<Value, FilesystemError> {
        let plaintext = self.read_encrypted_bytes(path)?;
        serde_json::from_slice(&plaintext).map_err(|_| FilesystemError::InvalidJson)
    }

    pub fn write_encrypted_text(&self, path: &Path, value: &str) -> Result<(), FilesystemError> {
        self.write_encrypted_bytes(path, value.as_bytes())
    }

    pub fn read_encrypted_text(&self, path: &Path) -> Result<Zeroizing<String>, FilesystemError> {
        let plaintext = self.read_encrypted_bytes(path)?;
        let value =
            String::from_utf8(plaintext.to_vec()).map_err(|_| FilesystemError::InvalidText)?;
        Ok(Zeroizing::new(value))
    }
}

fn expand_user(path: &Path) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    else {
        return path.to_path_buf();
    };
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map_or_else(|| path.to_path_buf(), |home| PathBuf::from(home).join(rest))
}

/// Build an absolute key path without following an existing symlink component.
fn safe_key_path(path: &Path) -> Result<PathBuf, FilesystemError> {
    let candidate =
        absolute(expand_user(path)).map_err(|_| FilesystemError::KeyDirectoryUnavailable)?;
    for parent in candidate.ancestors().skip(1) {
        match fs::symlink_metadata(parent) {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(FilesystemError::KeyDirectoryNotRegular);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(FilesystemError::KeyDirectoryUnavailable),
        }
    }
    match fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) => {
            Err(FilesystemError::KeyFileNotRegular)
        }
        Ok(_) => Ok(candidate),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(_) => Err(FilesystemError::KeyFileUnavailable),
    }
}

fn read_key_file(path: &Path) -> Result<FernetKey, FilesystemError> {
    let path = safe_key_path(path)?;
    let metadata = fs::symlink_metadata(&path).map_err(|_| FilesystemError::KeyFileUnavailable)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err(FilesystemError::KeyFileNotRegular);
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(&path)
        .map_err(|_| FilesystemError::KeyFileUnavailable)?;
    validate_private_key_file(&file, &metadata)?;
    let mut raw = Zeroizing::new(String::new());
    file.read_to_string(&mut raw)
        .map_err(|_| FilesystemError::KeyFileUnavailable)?;
    FernetKey::from_encoded(raw.trim()).map_err(|_| FilesystemError::KeyFileInvalid)
}

fn create_key_file(path: &Path) -> Result<(), FilesystemError> {
    let path = safe_key_path(path)?;
    let parent = path
        .parent()
        .ok_or(FilesystemError::KeyDirectoryUnavailable)?;
    let parent_existed = parent.exists();
    fs::create_dir_all(parent).map_err(|_| FilesystemError::KeyDirectoryUnavailable)?;
    safe_key_path(&path)?;
    if !parent_existed {
        set_private_directory(parent)?;
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(_) => return Err(FilesystemError::KeyFileCannotCreate),
    };
    if set_private_file_mode(&file, 0o600).is_err() {
        drop(file);
        let _ = fs::remove_file(&path);
        return Err(FilesystemError::KeyFileCannotCreate);
    }
    let key = FernetKey::generate();
    let result = file
        .write_all(key.encoded().as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&path);
        return Err(FilesystemError::KeyFileCannotCreate);
    }
    Ok(())
}

#[cfg(all(test, windows))]
#[path = "windows_tests.rs"]
mod windows_tests;
