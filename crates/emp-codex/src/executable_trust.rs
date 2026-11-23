//! Canonical regular executables require trusted owners and ancestors; Unix targets cannot be set-id.
//! Sticky entries and writable groups are trusted only when they exclude foreign users, including primary group members.
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
    #[cfg_attr(
        not(unix),
        allow(
            dead_code,
            reason = "Writable mode-bit checks are Unix-only; the error type is shared across platforms"
        )
    )]
    Writable,
    #[cfg(target_os = "macos")]
    BundleVerificationFailed,
}

pub(crate) fn validate_executable(path: &Path) -> Result<(), TrustFailure> {
    validate_executable_with(path, |_, _| false)
}

fn validate_executable_with(
    path: &Path,
    #[cfg_attr(
        not(unix),
        allow(
            unused_variables,
            reason = "Only the Unix ancestor permission check consumes this policy callback"
        )
    )]
    permit_writable_parent: impl Fn(&Path, &fs::Metadata) -> bool,
) -> Result<(), TrustFailure> {
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
        let mut entry_uid = metadata.uid();
        for parent in path.ancestors().skip(1) {
            let info = fs::metadata(parent).map_err(|_| TrustFailure::Unavailable)?;
            if foreign_writable(&info, Some(entry_uid), current_uid, &private_groups)
                && !permit_writable_parent(parent, &info)
            {
                return Err(TrustFailure::Writable);
            }
            entry_uid = info.uid();
        }
    }
    Ok(())
}

/// Validate a child PATH directory; sticky world-write protection cannot stop foreign commands.
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
    let sticky_protects_entry = mode & 0o1000 != 0
        && info.is_dir()
        && entry_uid.is_some_and(|uid| trusted_owner(uid, current_uid));
    if sticky_protects_entry {
        return false;
    }
    mode & 0o002 != 0 || (mode & 0o020 != 0 && !private_groups.contains(info.gid()))
}

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

/// Reject partial passwd enumerations that could miss group members; serialize getpwent's global cursor.
#[cfg(unix)]
fn enumerate_accounts() -> Option<Vec<(Vec<u8>, u32, u32)>> {
    static ENUMERATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ENUMERATION.lock().ok()?;
    // SAFETY: setpwent/getpwent/endpwent are called under the lock above;
    // each returned entry is copied before the next call invalidates it.
    unsafe {
        libc::setpwent();
        let accounts = collect_entries(|| {
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

/// Drain `next` (`Ok(None)` ends, `Ok(Some(None))` skips); discard partial or empty enumerations.
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
    let mut aligned_group_buffer = vec![0u64; 64 * 1024 / std::mem::size_of::<u64>()];
    // SAFETY: an all-zero group is a valid out-parameter for getgrgid_r.
    let mut entry: libc::group = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `aligned_group_buffer` outlives the
    // member strings copied out below.
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            &mut entry,
            aligned_group_buffer.as_mut_ptr().cast::<libc::c_char>(),
            aligned_group_buffer.len() * std::mem::size_of::<u64>(),
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
    // SAFETY: gr_mem is a NULL-terminated array of NUL-terminated strings in the live buffer;
    // read_unaligned handles member pointers that getgrgid_r may leave unaligned on macOS.
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
