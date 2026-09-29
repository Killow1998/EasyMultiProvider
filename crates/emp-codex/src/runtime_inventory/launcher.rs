//! Preserve the Node interpreter needed by npm-installed Codex launchers.
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PathAdjustment {
    Set(OsString),
    Inherit,
    Refuse,
}

/// Put a trusted Node runtime beside an npm launcher ahead of the inherited
/// PATH. npm's `codex` entry commonly has `#!/usr/bin/env node`; desktop apps
/// often start with a PATH that omits the nvm version containing both files.
/// If an untrusted sibling `node` exists, remove that directory from PATH so
/// `/usr/bin/env` cannot select it from an inherited PATH entry.
pub(crate) fn path_with_node(directory: &Path, inherited: Option<OsString>) -> PathAdjustment {
    let Ok(directory) = directory.canonicalize() else {
        return PathAdjustment::Inherit;
    };
    let node = directory.join(if cfg!(windows) { "node.exe" } else { "node" });
    let node_exists = fs::symlink_metadata(&node).is_ok();
    let trusted_node = crate::executable_trust::validate_search_directory(&directory).is_ok()
        && super::trust::trusted_binary(&node).is_some();
    if !trusted_node && !node_exists {
        return PathAdjustment::Inherit;
    }

    let mut paths = Vec::new();
    if trusted_node {
        paths.push(directory.clone());
    }
    if let Some(inherited) = inherited {
        for path in std::env::split_paths(&inherited) {
            let aliases_launcher = path == directory
                || path
                    .canonicalize()
                    .is_ok_and(|canonical| canonical == directory);
            if node_exists && !trusted_node && aliases_launcher {
                continue;
            }
            let already_present = paths.iter().any(|present: &PathBuf| {
                present == &path
                    || present
                        .canonicalize()
                        .is_ok_and(|canonical| path.canonicalize().is_ok_and(|p| p == canonical))
            });
            if !already_present {
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        return PathAdjustment::Refuse;
    }
    std::env::join_paths(paths).map_or(PathAdjustment::Refuse, PathAdjustment::Set)
}

/// Resolve an npm launcher's directory before its symlink target is
/// canonicalized, so its sibling `node` remains discoverable at spawn time.
pub(crate) fn launcher_directory(path: &Path) -> Option<PathBuf> {
    path.parent()?.canonicalize().ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::{PathAdjustment, path_with_node};
    use crate::runtime_inventory::trust::tests::{private_dir, script};
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn adds_only_a_trusted_sibling_node_to_the_child_path() {
        let root = private_dir();
        let bin = root.path().join("node/bin");
        std::fs::create_dir_all(&bin).unwrap();
        script(&bin, "node", "exit 0", 0o700);

        let PathAdjustment::Set(path) = path_with_node(&bin, Some(OsString::from("/usr/bin")))
        else {
            panic!("trusted sibling Node should produce a child PATH");
        };
        let entries = std::env::split_paths(&path).collect::<Vec<_>>();
        assert_eq!(entries[0], bin.canonicalize().unwrap());
        assert_eq!(entries.last().unwrap(), Path::new("/usr/bin"));

        std::fs::set_permissions(
            bin.join("node"),
            std::os::unix::fs::PermissionsExt::from_mode(0o777),
        )
        .unwrap();
        let PathAdjustment::Set(sanitized) = path_with_node(&bin, Some(path.clone())) else {
            panic!("safe inherited PATH should remain available");
        };
        assert!(std::env::split_paths(&sanitized).all(|entry| entry != bin));
    }

    #[test]
    fn trusted_node_symlink_does_not_trust_its_writable_path_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = private_dir();
        let trusted_dir = root.path().join("trusted");
        let writable_dir = root.path().join("writable");
        std::fs::create_dir(&trusted_dir).unwrap();
        std::fs::create_dir(&writable_dir).unwrap();
        let trusted_node = trusted_dir.join("node");
        std::fs::write(&trusted_node, b"not executed").unwrap();
        std::fs::set_permissions(&trusted_node, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&trusted_node, writable_dir.join("node")).unwrap();
        std::fs::set_permissions(&writable_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(crate::runtime_inventory::trust::trusted_binary(&trusted_node).is_some());

        let inherited = std::env::join_paths([&writable_dir, Path::new("/usr/bin")]).unwrap();
        let PathAdjustment::Set(path) = path_with_node(&writable_dir, Some(inherited)) else {
            panic!("/usr/bin should remain in the child PATH");
        };
        let entries = std::env::split_paths(&path).collect::<Vec<_>>();
        assert!(entries.iter().all(|entry| entry != &writable_dir));
        assert!(entries.contains(&Path::new("/usr/bin").to_path_buf()));
    }

    #[test]
    fn refuses_launch_when_removing_the_untrusted_launcher_dir_would_empty_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = private_dir();
        let writable_dir = root.path().join("writable");
        std::fs::create_dir(&writable_dir).unwrap();
        let node = writable_dir.join("node");
        std::fs::write(&node, b"not executed").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&writable_dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        let inherited = std::env::join_paths([&writable_dir]).unwrap();
        assert_eq!(
            path_with_node(&writable_dir, Some(inherited)),
            PathAdjustment::Refuse
        );
    }
}
