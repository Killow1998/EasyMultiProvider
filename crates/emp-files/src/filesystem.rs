use atomic_write_file::AtomicWriteFile;
use serde_json::Value;
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

pub const MAX_TRANSACTION_FILE_BYTES: usize = 64 * 1024 * 1024;
/// Upper bound for one encrypted vault file (credentials and provider keys).
pub const MAX_ENCRYPTED_FILE_BYTES: usize = 16 * 1024 * 1024;
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
    CredentialFileTooLarge,
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
            Self::CredentialFileTooLarge => {
                formatter.write_str("encrypted credential file is too large")
            }
        }
    }
}

impl std::error::Error for FilesystemError {}

/// Read at most `limit` bytes, returning `ErrorKind::FileTooLarge` for longer input.
pub fn read_to_limit(reader: &mut impl Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("file exceeds the {limit}-byte limit"),
        ));
    }
    Ok(data)
}

/// Read a bounded file; oversized inputs fail with `FileTooLarge`, other error kinds are preserved.
pub fn read_file_limited(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() > limit as u64 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("file exceeds the {limit}-byte limit"),
        ));
    }
    read_to_limit(&mut file, limit)
}

/// Open without following final links/reparse points or blocking on FIFOs/devices.
fn open_nofollow_for_read(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(path)
}

/// Create a real directory with mode 0700 on Unix or a protected current-user DACL on Windows.
/// Ancestors may be links because system paths such as macOS `/var` and `/tmp` use them.
pub fn create_private_directory(path: &Path) -> Result<(), FilesystemError> {
    let candidate = absolute(path).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    fs::create_dir_all(&candidate).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    let metadata =
        fs::symlink_metadata(&candidate).map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(FilesystemError::ManagedFileNotRegular);
    }
    set_private_directory(&candidate).map_err(|_| FilesystemError::ManagedFileUnavailable)
}

/// Create a file exclusively with mode 0600 or a protected current-user DACL, then write `data`.
/// Existing paths are never replaced or followed; partial writes are removed.
pub fn write_new_private_file(path: &Path, data: &[u8]) -> Result<(), FilesystemError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(path)
        .map_err(|_| FilesystemError::ManagedFileUnavailable)?;
    let result = set_private_file_mode(&file, 0o600).and_then(|()| {
        file.write_all(data)
            .and_then(|()| file.sync_all())
            .map_err(|_| FilesystemError::ManagedFileUnavailable)
    });
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

#[cfg(windows)]
#[doc(hidden)]
pub fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || crate::private_windows::metadata_is_reparse(metadata)
}

#[cfg(not(windows))]
#[doc(hidden)]
pub fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Reject existing links/reparse points in every target component before atomic replacement,
/// rechecking missing components after their parent directories are created.
#[doc(hidden)]
pub fn ensure_managed_write_path(path: &Path) -> Result<(), FilesystemError> {
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

#[cfg(unix)]
#[doc(hidden)]
pub fn validate_private_key_file(
    _: &fs::File,
    metadata: &fs::Metadata,
) -> Result<(), FilesystemError> {
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
#[doc(hidden)]
pub fn validate_private_key_file(file: &fs::File, _: &fs::Metadata) -> Result<(), FilesystemError> {
    crate::private_windows::validate_private_key_file(file)
}

#[cfg(not(any(unix, windows)))]
#[doc(hidden)]
pub fn validate_private_key_file(_: &fs::File, _: &fs::Metadata) -> Result<(), FilesystemError> {
    Ok(())
}

#[cfg(unix)]
#[doc(hidden)]
pub fn set_private_directory(path: &Path) -> Result<(), FilesystemError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| FilesystemError::KeyDirectoryUnavailable)
}

#[cfg(windows)]
#[doc(hidden)]
pub fn set_private_directory(path: &Path) -> Result<(), FilesystemError> {
    crate::private_windows::set_private_directory(path)
}

#[cfg(not(any(unix, windows)))]
#[doc(hidden)]
pub fn set_private_directory(_: &Path) -> Result<(), FilesystemError> {
    Ok(())
}

/// Atomically replace private configuration without changing caller-owned parent permissions.
pub fn atomic_write_config(path: &Path, data: &[u8]) -> Result<(), FilesystemError> {
    atomic_write(path, data, CONFIG_FILE_MODE, false)
}

/// Persist EMP-owned state with a private file and a private parent directory.
pub fn atomic_write_private_state(path: &Path, data: &[u8]) -> Result<(), FilesystemError> {
    atomic_write(path, data, CONFIG_FILE_MODE, true)
}

/// Atomically write a private catalog; the caller owns any enclosing transaction.
pub fn write_catalog_json(path: &Path, value: &Value) -> Result<(), FilesystemError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| FilesystemError::InvalidJson)?;
    bytes.push(b'\n');
    atomic_write_config(path, &bytes)
}

#[doc(hidden)]
pub fn atomic_write(
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
#[doc(hidden)]
pub fn set_private_file_mode(file: &fs::File, mode: u32) -> Result<(), FilesystemError> {
    file.set_permissions(fs::Permissions::from_mode(if mode == 0 {
        0o600
    } else {
        mode
    }))
    .map_err(|_| FilesystemError::ManagedFileUnavailable)
}

#[cfg(windows)]
#[doc(hidden)]
pub fn set_private_file_mode(file: &fs::File, _: u32) -> Result<(), FilesystemError> {
    crate::private_windows::set_private_file(file)
}

#[cfg(not(any(unix, windows)))]
#[doc(hidden)]
pub fn set_private_file_mode(_: &fs::File, _: u32) -> Result<(), FilesystemError> {
    Ok(())
}

struct Snapshot {
    path: PathBuf,
    data: Option<Zeroizing<Vec<u8>>>,
    mode: u32,
}

/// A bounded transaction whose uncommitted drop restores files in reverse order as panic-safe best effort.
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
                let mut file = open_nofollow_for_read(&path).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        FilesystemError::ManagedFileUnavailable
                    } else {
                        FilesystemError::ManagedFileNotRegular
                    }
                })?;
                let metadata = file
                    .metadata()
                    .map_err(|_| FilesystemError::ManagedFileUnavailable)?;
                if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
                    return Err(FilesystemError::ManagedFileNotRegular);
                }
                if metadata.len() > MAX_TRANSACTION_FILE_BYTES as u64 {
                    return Err(FilesystemError::ManagedFileTooLarge);
                }
                let data =
                    read_to_limit(&mut file, MAX_TRANSACTION_FILE_BYTES).map_err(|error| {
                        if error.kind() == std::io::ErrorKind::FileTooLarge {
                            FilesystemError::ManagedFileTooLarge
                        } else {
                            FilesystemError::ManagedFileUnavailable
                        }
                    })?;
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

/// Serialize in-process transactions and roll back on error, preserving higher-level failure types.
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
