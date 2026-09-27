//! Executable trust checks applied before any inventory candidate is run.
//!
//! The rule is shared with quota app-server processes; see
//! [`crate::executable_trust`].
use std::path::{Path, PathBuf};

/// Return the canonical path of `path` if it may be executed for probing.
pub(super) fn trusted_binary(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    crate::executable_trust::validate_executable(&canonical)
        .is_ok()
        .then_some(canonical)
}

#[cfg(all(test, unix))]
pub(super) mod tests {
    use super::trusted_binary;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A scratch directory whose ancestors are not world-writable (unlike /tmp).
    pub(in super::super) fn private_dir() -> tempfile::TempDir {
        let base = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let dir = tempfile::Builder::new()
            .prefix("emp-inventory-trust-")
            .tempdir_in(base)
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    pub(in super::super) fn script(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    pub(in super::super) fn ancestors_are_private(path: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: getuid has no preconditions and cannot fail.
        let uid = unsafe { libc::getuid() };
        path.ancestors().all(|p| {
            std::fs::metadata(p).is_ok_and(|m| {
                m.mode() & 0o002 == 0 && (m.mode() & 0o020 == 0 || m.uid() == 0 || m.uid() == uid)
            })
        })
    }

    #[test]
    fn accepts_private_executable() {
        let dir = private_dir();
        let path = script(dir.path(), "codex", "exit 0", 0o700);
        if !ancestors_are_private(dir.path()) {
            // Build directory lives under a shared-writable tree; nothing to assert.
            return;
        }
        assert_eq!(trusted_binary(&path), Some(path.canonicalize().unwrap()));
    }

    #[test]
    fn rejects_relative_non_executable_and_writable_locations() {
        let dir = private_dir();
        let plain = script(dir.path(), "plain", "exit 0", 0o600);
        assert_eq!(trusted_binary(&plain), None);
        assert_eq!(trusted_binary(Path::new("codex")), None);
        assert_eq!(trusted_binary(Path::new("./codex")), None);
        assert_eq!(trusted_binary(Path::new("")), None);

        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let hijack = script(&shared, "codex", "exit 0", 0o755);
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(trusted_binary(&hijack), None);

        // A private-looking symlink must not hide a binary in a writable directory.
        let link = dir.path().join("link-codex");
        std::os::unix::fs::symlink(&hijack, &link).unwrap();
        assert_eq!(trusted_binary(&link), None);
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();

        // Like /tmp: sticky and world-writable, but our own entry below it
        // cannot be renamed or removed by other users.
        let sticky = dir.path().join("sticky");
        std::fs::create_dir(&sticky).unwrap();
        let owned = script(&sticky, "codex", "exit 0", 0o755);
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();
        if ancestors_are_private(dir.path()) {
            assert_eq!(trusted_binary(&owned), Some(owned.canonicalize().unwrap()));
        }
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o700)).unwrap();

        let writable = script(dir.path(), "writable", "exit 0", 0o777);
        assert_eq!(trusted_binary(&writable), None);
        let set_id = script(dir.path(), "set-id", "exit 0", 0o4755);
        assert_eq!(trusted_binary(&set_id), None);
    }
}
