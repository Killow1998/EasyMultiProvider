//! The one executable trust rule for every Codex binary EMP runs: runtime
//! inventory probes and quota app-server processes.
//!
//! The canonical target must be a regular file. On Unix it must also be
//! executable, owned by root or the current user and not set-id, and neither
//! it nor any ancestor directory may be writable by another user:
//!
//! - world write is foreign unless the directory is sticky and the entry
//!   below it belongs to root or the current user (`/tmp`), since then other
//!   users cannot rename or remove that entry;
//! - group write is foreign unless the entry is owned by root or the current
//!   user and its group is private: the root group or the current user's
//!   primary group, with no supplementary member other than the current user
//!   (user-private groups from umask 002 npm/nvm installs).
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustFailure {
    /// The target or an ancestor cannot be inspected.
    Unavailable,
    /// The target itself is not an acceptable executable.
    NotTrusted,
    /// The target or an ancestor can be modified by another user.
    Writable,
}

/// Validate a canonical absolute executable path.
pub(crate) fn validate_executable(path: &Path) -> Result<(), TrustFailure> {
    let metadata = fs::metadata(path).map_err(|_| TrustFailure::Unavailable)?;
    if !metadata.is_file() {
        return Err(TrustFailure::NotTrusted);
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        let current_uid = unsafe { libc::getuid() };
        if metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o6000 != 0
            || !trusted_owner(metadata.uid(), current_uid)
        {
            return Err(TrustFailure::NotTrusted);
        }
        let private_groups = PrivateGroups::current(current_uid);
        if foreign_writable(&metadata, None, current_uid, &private_groups) {
            return Err(TrustFailure::Writable);
        }
        // Owner of the entry directly below the ancestor being checked.
        let mut entry_uid = metadata.uid();
        for parent in path.ancestors().skip(1) {
            let info = fs::metadata(parent).map_err(|_| TrustFailure::Unavailable)?;
            if foreign_writable(&info, Some(entry_uid), current_uid, &private_groups) {
                return Err(TrustFailure::Writable);
            }
            entry_uid = info.uid();
        }
    }
    Ok(())
}

#[cfg(unix)]
fn trusted_owner(uid: u32, current_uid: u32) -> bool {
    uid == 0 || uid == current_uid
}

/// `entry_uid` is the owner of the entry below a directory; `None` for the
/// target file itself.
#[cfg(unix)]
fn foreign_writable(
    info: &fs::Metadata,
    entry_uid: Option<u32>,
    current_uid: u32,
    private_groups: &PrivateGroups,
) -> bool {
    let mode = info.mode();
    let sticky_protects_entry = mode & 0o1000 != 0
        && info.is_dir()
        && entry_uid.is_some_and(|uid| trusted_owner(uid, current_uid));
    if mode & 0o002 != 0 && !sticky_protects_entry {
        return true;
    }
    mode & 0o020 != 0
        && !(trusted_owner(info.uid(), current_uid) && private_groups.contains(info.gid()))
        && !sticky_protects_entry
}

/// Groups whose write permission grants no other user access.
#[cfg(unix)]
struct PrivateGroups {
    current_uid: u32,
    primary_gid: Option<u32>,
    user_name: Option<Vec<u8>>,
}

#[cfg(unix)]
impl PrivateGroups {
    fn current(current_uid: u32) -> Self {
        let (primary_gid, user_name) = match passwd_entry(current_uid) {
            Some((gid, name)) => (Some(gid), Some(name)),
            None => (None, None),
        };
        Self {
            current_uid,
            primary_gid,
            user_name,
        }
    }

    fn contains(&self, gid: u32) -> bool {
        let expected = if self.current_uid == 0 {
            gid == 0
        } else {
            gid == 0 || Some(gid) == self.primary_gid
        };
        if !expected {
            return false;
        }
        group_members(gid).is_some_and(|members| {
            members
                .iter()
                .all(|member| self.user_name.as_deref() == Some(member.as_slice()))
        })
    }
}

#[cfg(unix)]
fn passwd_entry(uid: u32) -> Option<(u32, Vec<u8>)> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero passwd is a valid out-parameter for getpwuid_r.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buffer` outlives the
    // strings copied out of `entry` below.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: getpwuid_r succeeded, so pw_name is a NUL-terminated string
    // inside `buffer`.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
        .to_bytes()
        .to_vec();
    Some((entry.pw_gid, name))
}

/// Supplementary members of `gid`; `None` when the group cannot be resolved.
#[cfg(unix)]
fn group_members(gid: u32) -> Option<Vec<Vec<u8>>> {
    let mut buffer = vec![0 as libc::c_char; 64 * 1024];
    // SAFETY: an all-zero group is a valid out-parameter for getgrgid_r.
    let mut entry: libc::group = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buffer` outlives the
    // member strings copied out below.
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let mut members = Vec::new();
    let mut cursor = entry.gr_mem;
    if cursor.is_null() {
        return Some(members);
    }
    // SAFETY: gr_mem is a NULL-terminated array of NUL-terminated strings
    // inside `buffer`.
    unsafe {
        while !(*cursor).is_null() {
            members.push(std::ffi::CStr::from_ptr(*cursor).to_bytes().to_vec());
            cursor = cursor.add(1);
        }
    }
    Some(members)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn root_group_without_members_is_private() {
        // Every Unix system resolves gid 0; containers and CI list no members.
        if group_members(0).is_some_and(|members| members.is_empty()) {
            // SAFETY: getuid has no preconditions and cannot fail.
            assert!(PrivateGroups::current(unsafe { libc::getuid() }).contains(0));
        }
    }

    #[test]
    fn unrelated_groups_are_not_private() {
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        let groups = PrivateGroups::current(uid);
        let unrelated = (1..65_534)
            .find(|gid| Some(*gid) != groups.primary_gid && group_members(*gid).is_some());
        if let Some(gid) = unrelated {
            assert!(!groups.contains(gid), "gid {gid}");
        }
    }

    #[test]
    fn group_write_is_foreign_outside_private_groups() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let dir = tempfile::Builder::new()
            .prefix("emp-executable-trust-")
            .tempdir_in(base)
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("codex");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o775)).unwrap();
        let path = path.canonicalize().unwrap();
        let ancestors_clean = path.ancestors().skip(1).all(|ancestor| {
            std::fs::metadata(ancestor)
                .is_ok_and(|info| info.mode() & 0o022 == 0 || validate_executable(ancestor).is_ok())
        });
        // SAFETY: getuid has no preconditions and cannot fail.
        let groups = PrivateGroups::current(unsafe { libc::getuid() });
        let file_gid = std::fs::metadata(&path).unwrap().gid();
        if ancestors_clean && groups.contains(file_gid) {
            assert_eq!(validate_executable(&path), Ok(()));
        }
        // A supplementary group of the current user that is not private.
        // SAFETY: a zero-length query only returns the group count.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        let mut supplementary = vec![0 as libc::gid_t; count.max(0) as usize];
        // SAFETY: the buffer holds `count` entries.
        let filled = unsafe { libc::getgroups(count, supplementary.as_mut_ptr()) };
        supplementary.truncate(filled.max(0) as usize);
        let shared = supplementary.into_iter().find(|gid| !groups.contains(*gid));
        if let Some(gid) = shared {
            let target = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: `target` is a valid path; uid -1 keeps the owner.
            assert_eq!(unsafe { libc::chown(target.as_ptr(), u32::MAX, gid) }, 0);
            assert_eq!(validate_executable(&path), Err(TrustFailure::Writable));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            if ancestors_clean {
                assert_eq!(validate_executable(&path), Ok(()));
            }
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4755)).unwrap();
        assert_eq!(validate_executable(&path), Err(TrustFailure::NotTrusted));
    }
}
