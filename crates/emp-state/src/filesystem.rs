//! Private master-key storage, encrypted vault files and bounded rollback.
//!
//! The format remains compatible with easy_multi_provider.vault. Runtime
//! composition constructs one VaultStore and passes it explicitly, avoiding
//! process-global key-path state and environment races.

use crate::{FernetKey, VaultError, decode_vault, encode_vault};
use atomic_write_file::AtomicWriteFile;
use serde_json::Value;
use std::env;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf, absolute};
use std::sync::{LazyLock, Mutex};
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt as StdOpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as StdOpenOptionsExt;

pub const MASTER_KEY_ENV: &str = "EASY_MULTI_PROVIDER_MASTER_KEY";
pub const MASTER_KEY_FILE_ENV: &str = "EASY_MULTI_PROVIDER_MASTER_KEY_FILE";
pub const MAX_TRANSACTION_FILE_BYTES: usize = 64 * 1024 * 1024;
const CONFIG_FILE_MODE: u32 = 0o600;

static FILE_TRANSACTION_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemError {
    ManagedFileUnavailable,
    ManagedFileNotRegular,
    ManagedFileTooLarge,
    ManagedFileRollbackFailed,
    KeyDirectoryUnavailable,
    KeyDirectoryNotRegular,
    KeyFileUnavailable,
    KeyFileNotRegular,
    KeyFileNotOwned,
    KeyFileNotPrivate,
    EnvironmentKeyInvalid,
    KeyFileInvalid,
    KeyFileCannotCreate,
    CredentialFileUnavailable,
    UnsupportedCredentialFormat,
    CredentialDecryptFailed,
    InvalidJson,
    InvalidText,
}

impl fmt::Display for FilesystemError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManagedFileUnavailable => formatter.write_str("managed file is unavailable"),
            Self::ManagedFileNotRegular => {
                formatter.write_str("managed file is not a regular file")
            }
            Self::ManagedFileTooLarge => {
                formatter.write_str("managed file is too large for an atomic update")
            }
            Self::ManagedFileRollbackFailed => formatter.write_str("managed file rollback failed"),
            Self::KeyDirectoryUnavailable => {
                formatter.write_str("master key directory is unavailable")
            }
            Self::KeyDirectoryNotRegular => {
                formatter.write_str("master key directory is not a regular directory")
            }
            Self::KeyFileUnavailable => formatter.write_str("master key file is unavailable"),
            Self::KeyFileNotRegular => formatter.write_str("master key file is not a regular file"),
            Self::KeyFileNotOwned => {
                formatter.write_str("master key file is not owned by the current user")
            }
            Self::KeyFileNotPrivate => formatter.write_str("master key file must be private"),
            Self::EnvironmentKeyInvalid => {
                formatter.write_str("EASY_MULTI_PROVIDER_MASTER_KEY is not a valid Fernet key")
            }
            Self::KeyFileInvalid => {
                formatter.write_str("master key file is not a valid Fernet key")
            }
            Self::KeyFileCannotCreate => formatter.write_str("master key file cannot be created"),
            Self::CredentialFileUnavailable => {
                formatter.write_str("encrypted credential file is unavailable")
            }
            Self::UnsupportedCredentialFormat => {
                formatter.write_str("credential file is not encrypted with the supported format")
            }
            Self::CredentialDecryptFailed => formatter
                .write_str("credential file cannot be decrypted with the current master key"),
            Self::InvalidJson => {
                formatter.write_str("encrypted credential content is invalid JSON")
            }
            Self::InvalidText => {
                formatter.write_str("encrypted credential content is not valid text")
            }
        }
    }
}

impl std::error::Error for FilesystemError {}

impl From<VaultError> for FilesystemError {
    fn from(error: VaultError) -> Self {
        match error {
            VaultError::NotVaultFormat => Self::UnsupportedCredentialFormat,
            VaultError::DecryptFailed => Self::CredentialDecryptFailed,
        }
    }
}

/// One validated master key and its optional durable location.
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

    /// Construct from explicit sources for deterministic service composition.
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
        let mut raw = Vec::new();
        file.read_to_end(&mut raw)
            .map_err(|_| FilesystemError::CredentialFileUnavailable)?;
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

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || crate::private_windows::metadata_is_reparse(metadata)
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Reject existing links/reparse points in every target component before an
/// atomic replacement. Missing components are allowed and checked again after
/// their parent directories have been created.
fn ensure_managed_write_path(path: &Path) -> Result<(), FilesystemError> {
    let candidate = absolute(path).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    for parent in candidate.ancestors().skip(1) {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
                return Err(FilesystemError::ManagedFileNotRegular);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(FilesystemError::ManagedFileUnavailable),
        }
    }
    match fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            Err(FilesystemError::ManagedFileNotRegular)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(FilesystemError::ManagedFileUnavailable),
    }
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

#[cfg(unix)]
fn validate_private_key_file(_: &fs::File, metadata: &fs::Metadata) -> Result<(), FilesystemError> {
    // SAFETY: getuid has no preconditions and does not dereference pointers.
    let current_uid = unsafe { libc::getuid() };
    if metadata.uid() != 0 && metadata.uid() != current_uid {
        return Err(FilesystemError::KeyFileNotOwned);
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(FilesystemError::KeyFileNotPrivate);
    }
    Ok(())
}

#[cfg(windows)]
fn validate_private_key_file(file: &fs::File, _: &fs::Metadata) -> Result<(), FilesystemError> {
    crate::private_windows::validate_private_key_file(file)
}

#[cfg(not(any(unix, windows)))]
fn validate_private_key_file(_: &fs::File, _: &fs::Metadata) -> Result<(), FilesystemError> {
    Ok(())
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

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<(), FilesystemError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| FilesystemError::KeyDirectoryUnavailable)
}

#[cfg(windows)]
fn set_private_directory(path: &Path) -> Result<(), FilesystemError> {
    crate::private_windows::set_private_directory(path)
}

#[cfg(not(any(unix, windows)))]
fn set_private_directory(_: &Path) -> Result<(), FilesystemError> {
    Ok(())
}

/// Atomically replace a private configuration file without changing the
/// permissions of a caller-owned parent directory.
pub(crate) fn atomic_write_config(path: &Path, data: &[u8]) -> Result<(), FilesystemError> {
    atomic_write(path, data, CONFIG_FILE_MODE, false)
}

fn atomic_write(
    path: &Path,
    data: &[u8],
    mode: u32,
    private_parent: bool,
) -> Result<(), FilesystemError> {
    ensure_managed_write_path(path)?;
    let parent = path
        .parent()
        .ok_or(FilesystemError::ManagedFileUnavailable)?;
    fs::create_dir_all(parent).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    ensure_managed_write_path(path)?;
    if private_parent {
        set_private_directory(parent).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    }

    let mut file =
        AtomicWriteFile::open(path).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    set_private_file_mode(file.as_file(), mode)?;
    file.write_all(data)
        .and_then(|()| file.sync_all())
        .map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    file.commit()
        .map_err(|_| FilesystemError::ManagedFileUnavailable)
}

#[cfg(unix)]
fn set_private_file_mode(file: &fs::File, mode: u32) -> Result<(), FilesystemError> {
    file.set_permissions(fs::Permissions::from_mode(if mode == 0 {
        0o600
    } else {
        mode
    }))
    .map_err(|_| FilesystemError::ManagedFileUnavailable)
}

#[cfg(windows)]
fn set_private_file_mode(file: &fs::File, _: u32) -> Result<(), FilesystemError> {
    crate::private_windows::set_private_file(file)
}

#[cfg(not(any(unix, windows)))]
fn set_private_file_mode(_: &fs::File, _: u32) -> Result<(), FilesystemError> {
    Ok(())
}

struct Snapshot {
    path: PathBuf,
    data: Option<Zeroizing<Vec<u8>>>,
    mode: u32,
}

/// A bounded transaction. Unless committed, dropping it restores remembered
/// files in reverse order as a panic-safe best effort.
pub struct FileTransaction {
    snapshots: Vec<Snapshot>,
    active: bool,
}

impl FileTransaction {
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            active: true,
        }
    }

    pub fn remember(&mut self, path: &Path) -> Result<(), FilesystemError> {
        let path = absolute(path).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
        if self.snapshots.iter().any(|snapshot| snapshot.path == path) {
            return Ok(());
        }
        let snapshot = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                    return Err(FilesystemError::ManagedFileNotRegular);
                }
                if metadata.len() > MAX_TRANSACTION_FILE_BYTES as u64 {
                    return Err(FilesystemError::ManagedFileTooLarge);
                }
                let mut data = Vec::new();
                fs::File::open(&path)
                    .map_err(|_| FilesystemError::ManagedFileUnavailable)?
                    .take(MAX_TRANSACTION_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut data)
                    .map_err(|_| FilesystemError::ManagedFileUnavailable)?;
                if data.len() > MAX_TRANSACTION_FILE_BYTES {
                    return Err(FilesystemError::ManagedFileTooLarge);
                }
                Snapshot {
                    path,
                    data: Some(Zeroizing::new(data)),
                    mode: file_mode(&metadata),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Snapshot {
                path,
                data: None,
                mode: 0o600,
            },
            Err(_) => return Err(FilesystemError::ManagedFileUnavailable),
        };
        self.snapshots.push(snapshot);
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<(), FilesystemError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        for snapshot in self.snapshots.iter().rev() {
            let result = match &snapshot.data {
                Some(data) => atomic_write(&snapshot.path, data, snapshot.mode, false),
                None => match fs::remove_file(&snapshot.path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(_) => Err(FilesystemError::ManagedFileRollbackFailed),
                },
            };
            if result.is_err() {
                return Err(FilesystemError::ManagedFileRollbackFailed);
            }
        }
        self.snapshots.clear();
        Ok(())
    }

    pub fn commit(&mut self) {
        self.active = false;
        self.snapshots.clear();
    }
}

impl Default for FileTransaction {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if self.active {
            let _ = self.rollback();
        }
    }
}

#[cfg(unix)]
fn file_mode(metadata: &fs::Metadata) -> u32 {
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_: &fs::Metadata) -> u32 {
    0o600
}

/// Serialize transactions within the process and roll back when the operation
/// returns an error. The generic error can retain a higher-level failure.
pub fn with_file_transaction<T, E>(
    operation: impl FnOnce(&mut FileTransaction) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<FilesystemError>,
{
    let _guard = FILE_TRANSACTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut transaction = FileTransaction::new();
    match operation(&mut transaction) {
        Ok(value) => {
            transaction.commit();
            Ok(value)
        }
        Err(error) => {
            transaction.rollback().map_err(E::from)?;
            Err(error)
        }
    }
}
