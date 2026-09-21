//! Cross-process locks compatible with Python's integration lock files.

use std::env;
#[cfg(unix)]
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf, absolute};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{ERROR_IO_PENDING, ERROR_LOCK_VIOLATION, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFileEx,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::OVERLAPPED;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockError {
    PathUnsafe,
    OpenFailed,
    AcquireFailed,
    TimedOut(Duration),
    Unsupported,
}

impl fmt::Display for LockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathUnsafe => formatter.write_str("integration lock path is unsafe"),
            Self::OpenFailed => formatter.write_str("unable to open integration lock"),
            Self::AcquireFailed => formatter.write_str("unable to acquire integration lock"),
            Self::TimedOut(timeout) => write!(
                formatter,
                "integration lock timed out after {:.3} seconds",
                timeout.as_secs_f64()
            ),
            Self::Unsupported => {
                formatter.write_str("this platform has no supported file-lock primitive")
            }
        }
    }
}

impl std::error::Error for LockError {}

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

fn inspect_lock_path(path: &Path) -> Result<PathBuf, LockError> {
    let candidate = absolute(expand_user(path)).map_err(|_| LockError::PathUnsafe)?;
    let parent = candidate.parent().ok_or(LockError::PathUnsafe)?;
    let mut components = parent.ancestors().collect::<Vec<_>>();
    components.reverse();
    for component in components {
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_dir() => {
                return Err(LockError::PathUnsafe);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(LockError::PathUnsafe),
        }
    }
    match fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() => {
            Err(LockError::PathUnsafe)
        }
        Ok(_) => Ok(candidate),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(_) => Err(LockError::PathUnsafe),
    }
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() || crate::private_windows::metadata_is_reparse(metadata)
}

#[cfg(not(windows))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn open_lock_file(path: &Path) -> Result<File, LockError> {
    let path = inspect_lock_path(path)?;
    let parent = path.parent().ok_or(LockError::PathUnsafe)?;
    fs::create_dir_all(parent).map_err(|_| LockError::OpenFailed)?;
    let path = inspect_lock_path(&path)?;

    #[cfg(unix)]
    let file = {
        let mut parent_options = fs::OpenOptions::new();
        parent_options
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
        let parent_file = parent_options
            .open(parent)
            .map_err(|_| LockError::OpenFailed)?;
        parent_file
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|_| LockError::OpenFailed)?;
        let name = path.file_name().ok_or(LockError::PathUnsafe)?;
        let name = CString::new(name.as_bytes()).map_err(|_| LockError::PathUnsafe)?;
        // SAFETY: `parent_file` and `name` remain live through the call. The
        // returned descriptor is exclusively transferred into `File`.
        let descriptor = unsafe {
            libc::openat(
                parent_file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(LockError::OpenFailed);
        }
        // SAFETY: `openat` returned a new owned descriptor on success.
        unsafe { File::from_raw_fd(descriptor) }
    };

    #[cfg(windows)]
    let file = {
        crate::private_windows::set_private_directory(parent).map_err(|_| LockError::OpenFailed)?;
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        let file = options.open(&path).map_err(|_| LockError::OpenFailed)?;
        crate::private_windows::set_private_file(&file).map_err(|_| LockError::OpenFailed)?;
        file
    };

    #[cfg(not(any(unix, windows)))]
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| LockError::OpenFailed)?;
    let metadata = file.metadata().map_err(|_| LockError::OpenFailed)?;
    if !metadata.is_file() {
        return Err(LockError::OpenFailed);
    }
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| LockError::OpenFailed)?;
    if metadata.len() == 0 {
        file.set_len(1).map_err(|_| LockError::OpenFailed)?;
        file.sync_data().map_err(|_| LockError::OpenFailed)?;
    }
    Ok(file)
}

enum TryLock {
    Acquired,
    Contended,
}

#[cfg(unix)]
fn try_lock(file: &File) -> Result<TryLock, LockError> {
    // SAFETY: the descriptor remains owned by `file` for the call duration.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(TryLock::Acquired);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code == libc::EACCES || code == libc::EAGAIN => Ok(TryLock::Contended),
        _ => Err(LockError::AcquireFailed),
    }
}

#[cfg(windows)]
fn try_lock(file: &File) -> Result<TryLock, LockError> {
    let mut overlapped = OVERLAPPED::default();
    let handle = file.as_raw_handle() as HANDLE;
    // SAFETY: `handle` is live, the byte range and zeroed synchronous
    // OVERLAPPED are valid, and the pointer lives through the call.
    let locked = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(TryLock::Acquired);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(code) if code as u32 == ERROR_LOCK_VIOLATION || code as u32 == ERROR_IO_PENDING => {
            Ok(TryLock::Contended)
        }
        _ => Err(LockError::AcquireFailed),
    }
}

#[cfg(not(any(unix, windows)))]
fn try_lock(_: &File) -> Result<TryLock, LockError> {
    Err(LockError::Unsupported)
}

#[cfg(unix)]
fn unlock(file: &File) {
    // SAFETY: the descriptor is live until this function returns.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
fn unlock(file: &File) {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: this unlocks the same live handle and byte range acquired above.
    let _ = unsafe { UnlockFileEx(file.as_raw_handle() as HANDLE, 0, 1, 0, &mut overlapped) };
}

#[cfg(not(any(unix, windows)))]
fn unlock(_: &File) {}

/// An exclusive lock released when dropped. The underlying byte range matches
/// Python's `flock` on POSIX and `msvcrt.locking(..., 1)` on Windows.
pub struct IntegrationFileLock {
    file: File,
}

impl IntegrationFileLock {
    pub fn acquire(
        path: &Path,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, LockError> {
        let file = open_lock_file(path)?;
        let started = Instant::now();
        let poll_interval = poll_interval.max(Duration::from_millis(1));
        loop {
            match try_lock(&file)? {
                TryLock::Acquired => return Ok(Self { file }),
                TryLock::Contended if started.elapsed() >= timeout => {
                    return Err(LockError::TimedOut(timeout));
                }
                TryLock::Contended => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    thread::sleep(poll_interval.min(remaining));
                }
            }
        }
    }
}

impl Drop for IntegrationFileLock {
    fn drop(&mut self) {
        unlock(&self.file);
    }
}
