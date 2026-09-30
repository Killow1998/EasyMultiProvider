//! The one executable trust rule for every Codex binary EMP runs: runtime
//! inventory probes and quota app-server processes.
//!
//! The canonical target must be a regular file. On Unix it must also be
//! executable and not set-id, and neither it nor any ancestor directory may
//! be modifiable by another user:
//!
//! - every entry must be owned by root or the current user, since an owner
//!   can always change its entry's permissions;
//! - world write is foreign unless the directory is sticky and the entry
//!   below it belongs to root or the current user (`/tmp`), since then other
//!   users cannot rename or remove that entry;
//! - group write is foreign unless the group is private: every member, both
//!   users whose primary group it is and its supplementary members, is root
//!   or the current user (user-private groups from umask 002 npm/nvm
//!   installs).
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[cfg(any(target_os = "macos", all(test, unix)))]
mod macos;
mod prepared;
pub use prepared::PreparedExecutable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustFailure {
    /// The target or an ancestor cannot be inspected.
    Unavailable,
    /// The target itself is not an acceptable executable.
    NotTrusted,
    /// The target or an ancestor can be modified by another user.
    #[cfg_attr(not(unix), allow(dead_code))]
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

/// Validate a directory that will be placed on a child process PATH.
/// Unlike an executable's ancestors, the directory itself cannot use sticky
/// world-write protection: other users could add a higher-priority command.
pub(crate) fn validate_search_directory(path: &Path) -> Result<(), TrustFailure> {
    if !path.is_absolute() {
        return Err(TrustFailure::NotTrusted);
    }
    let canonical = path.canonicalize().map_err(|_| TrustFailure::Unavailable)?;
    let metadata = fs::metadata(&canonical).map_err(|_| TrustFailure::Unavailable)?;
    if !metadata.is_dir() {
        return Err(TrustFailure::NotTrusted);
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        let current_uid = unsafe { libc::getuid() };
        if !trusted_owner(metadata.uid(), current_uid) {
            return Err(TrustFailure::NotTrusted);
        }
        let private_groups = PrivateGroups::current(current_uid);
        if foreign_writable(&metadata, None, current_uid, &private_groups) {
            return Err(TrustFailure::Writable);
        }
        let mut entry_uid = metadata.uid();
        for parent in canonical.ancestors().skip(1) {
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
    if !trusted_owner(info.uid(), current_uid) {
        return true;
    }
    let mode = info.mode();
    // The directory owner is trusted (checked above), so only root, the
    // current user and the entry's owner can rename or remove the entry.
    let sticky_protects_entry = mode & 0o1000 != 0
        && info.is_dir()
        && entry_uid.is_some_and(|uid| trusted_owner(uid, current_uid));
    if sticky_protects_entry {
        return false;
    }
    mode & 0o002 != 0 || (mode & 0o020 != 0 && !private_groups.contains(info.gid()))
}

/// Groups whose write permission grants no other user access.
#[cfg(unix)]
struct PrivateGroups {
    current_uid: u32,
    /// `(name, uid, primary gid)` of every enumerable account; `None` when
    /// the account database cannot be enumerated.
    accounts: Option<Vec<(Vec<u8>, u32, u32)>>,
}

#[cfg(unix)]
impl PrivateGroups {
    fn current(current_uid: u32) -> Self {
        Self {
            current_uid,
            accounts: enumerate_accounts(),
        }
    }

    fn contains(&self, gid: u32) -> bool {
        self.contains_with_members(gid, group_members(gid))
    }

    fn contains_with_members(&self, gid: u32, members: Option<Vec<Vec<u8>>>) -> bool {
        let Some(accounts) = &self.accounts else {
            return false;
        };
        let trusted = |uid: u32| trusted_owner(uid, self.current_uid);
        // Users whose primary group this is are members without appearing
        // in the group's member list.
        if accounts
            .iter()
            .any(|(_, uid, primary)| *primary == gid && !trusted(*uid))
        {
            return false;
        }
        members.is_some_and(|members| {
            members.iter().all(|member| {
                accounts
                    .iter()
                    .find(|(name, _, _)| name == member)
                    .is_some_and(|(_, uid, _)| trusted(*uid))
            })
        })
    }
}

/// Every account the passwd database enumerates; `None` when the
/// enumeration fails part-way, since a missed account could share the group.
/// `getpwent` keeps global cursor state, so enumerations are serialized.
#[cfg(unix)]
fn enumerate_accounts() -> Option<Vec<(Vec<u8>, u32, u32)>> {
    static ENUMERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ENUMERATION.lock().ok()?;
    // SAFETY: setpwent/getpwent/endpwent are called under the lock above;
    // each returned entry is copied before the next call invalidates it.
    unsafe {
        libc::setpwent();
        let accounts = collect_entries(|| {
            // getpwent signals both the end and an error with NULL; only
            // errno tells them apart.
            *errno_location() = 0;
            let entry = libc::getpwent();
            if entry.is_null() {
                let errno = *errno_location();
                return if errno == 0 || errno == libc::ENOENT {
                    Ok(None)
                } else {
                    Err(())
                };
            }
            let entry = &*entry;
            if entry.pw_name.is_null() {
                return Ok(Some(None));
            }
            let name = std::ffi::CStr::from_ptr(entry.pw_name).to_bytes().to_vec();
            Ok(Some(Some((name, entry.pw_uid, entry.pw_gid))))
        });
        libc::endpwent();
        accounts
    }
}

/// Drain `next` (`Ok(None)` at the end, `Ok(Some(None))` for a skipped
/// entry). Any error discards everything: a partial list is not a complete
/// one. An empty enumeration is treated as a failure too.
#[cfg(unix)]
fn collect_entries<T>(mut next: impl FnMut() -> Result<Option<Option<T>>, ()>) -> Option<Vec<T>> {
    let mut entries = Vec::new();
    while let Some(entry) = next().ok()? {
        entries.extend(entry);
    }
    (!entries.is_empty()).then_some(entries)
}

#[cfg(target_os = "linux")]
use libc::__errno_location as errno_location;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
use libc::__error as errno_location;

/// Supplementary members of `gid`; `None` when the group cannot be resolved.
#[cfg(unix)]
fn group_members(gid: u32) -> Option<Vec<Vec<u8>>> {
    // getgrgid_r places the gr_mem pointer array inside `buffer`; macOS does
    // not realign it, so allocate words and still read members unaligned.
    let mut buffer = vec![0u64; 64 * 1024 / std::mem::size_of::<u64>()];
    // SAFETY: an all-zero group is a valid out-parameter for getgrgid_r.
    let mut entry: libc::group = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buffer` outlives the
    // member strings copied out below.
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            &mut entry,
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len() * std::mem::size_of::<u64>(),
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
        loop {
            let member = cursor.read_unaligned();
            if member.is_null() {
                break;
            }
            members.push(std::ffi::CStr::from_ptr(member).to_bytes().to_vec());
            cursor = cursor.add(1);
        }
    }
    Some(members)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn groups(current_uid: u32, accounts: &[(&str, u32, u32)]) -> PrivateGroups {
        PrivateGroups {
            current_uid,
            accounts: Some(
                accounts
                    .iter()
                    .map(|(name, uid, gid)| (name.as_bytes().to_vec(), *uid, *gid))
                    .collect(),
            ),
        }
    }

    fn names(members: &[&str]) -> Option<Vec<Vec<u8>>> {
        Some(
            members
                .iter()
                .map(|name| name.as_bytes().to_vec())
                .collect(),
        )
    }

    #[test]
    fn a_group_is_private_only_when_every_member_is_trusted() {
        let accounts = [("root", 0, 0), ("me", 1000, 1000), ("bob", 1001, 100)];
        let private = groups(1000, &accounts);
        // User-private group and the root group.
        assert!(private.contains_with_members(1000, names(&[])));
        assert!(private.contains_with_members(0, names(&["root"])));
        assert!(private.contains_with_members(1000, names(&["me"])));
        // Another user listed as a supplementary member.
        assert!(!private.contains_with_members(1000, names(&["bob"])));
        // Another user whose primary group it is, absent from gr_mem.
        assert!(!private.contains_with_members(100, names(&[])));
        let shared_primary = groups(1000, &[("me", 1000, 1000), ("eve", 1002, 1000)]);
        assert!(!shared_primary.contains_with_members(1000, names(&[])));
        // Unknown members, unresolvable groups and an unenumerable account
        // database are never private.
        assert!(!private.contains_with_members(1000, names(&["ghost"])));
        assert!(!private.contains_with_members(1000, None));
        let unknown = PrivateGroups {
            current_uid: 1000,
            accounts: None,
        };
        assert!(!unknown.contains_with_members(1000, names(&[])));
    }

    #[test]
    fn an_enumeration_that_fails_part_way_is_not_a_complete_one() {
        let run = |steps: Vec<Result<Option<Option<u32>>, ()>>| {
            let mut steps = steps.into_iter();
            collect_entries(|| steps.next().unwrap_or(Ok(None)))
        };
        assert_eq!(
            run(vec![
                Ok(Some(Some(0))),
                Ok(Some(None)),
                Ok(Some(Some(1000)))
            ]),
            Some(vec![0, 1000])
        );
        // A read error after some entries (getpwent NULL with errno set)
        // could hide another user sharing the group.
        assert_eq!(
            run(vec![Ok(Some(Some(0))), Err(()), Ok(Some(Some(1)))]),
            None
        );
        assert_eq!(run(vec![]), None);
        // The real database enumerates cleanly on this machine.
        assert!(
            enumerate_accounts()
                .is_some_and(|accounts| { accounts.iter().any(|(_, uid, _)| *uid == 0) })
        );
    }

    #[test]
    fn entries_owned_by_another_user_are_foreign_whatever_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let info = std::fs::metadata(dir.path()).unwrap();
        let owner = info.uid();
        let nobody_groups = groups(owner, &[]);
        assert!(!foreign_writable(&info, None, owner, &nobody_groups));
        // Seen from another (non-root) user, the owner can chmod or replace
        // entries at will, even without group or other write.
        if owner != 0 {
            let other = owner.wrapping_add(7919);
            assert!(foreign_writable(&info, None, other, &groups(other, &[])));
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
