//! Unix path and nvm discovery regressions.
use super::{RuntimePlatform, discover, nvm_installations, nvm_roots, path_cli_in};
use crate::runtime_inventory::trust::tests::{ancestors_are_private, private_dir, script};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

fn relative_to_cwd(target: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let target = target.canonicalize().unwrap();
    let mut relative = PathBuf::new();
    for component in cwd.components() {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    relative.join(target.strip_prefix("/").unwrap())
}

#[test]
fn path_cli_skips_empty_and_relative_entries() {
    let dir = private_dir();
    script(dir.path(), "codex", "exit 0", 0o755);
    let relative = relative_to_cwd(dir.path());
    assert!(relative.is_relative() && relative.join("codex").is_file());
    let path_var = std::env::join_paths([PathBuf::new(), relative, PathBuf::from(".")]).unwrap();
    assert_eq!(path_cli_in(&path_var), None);
    assert_eq!(path_cli_in(&OsString::new()), None);
}

#[test]
fn path_cli_skips_untrusted_directories() {
    use std::os::unix::fs::PermissionsExt;
    let dir = private_dir();
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    script(&shared, "codex", "exit 0", 0o755);
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let path_var = std::env::join_paths([shared.clone()]).unwrap();
    assert_eq!(path_cli_in(&path_var), None);
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[test]
fn path_cli_accepts_a_trusted_symlinked_installation() {
    use std::os::unix::fs::symlink;
    let dir = private_dir();
    if !ancestors_are_private(dir.path()) {
        return;
    }
    let install = dir.path().join("package");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&install).unwrap();
    std::fs::create_dir(&bin).unwrap();
    let target = script(&install, "codex", "echo codex-cli 0.156.1", 0o755);
    let link = bin.join("codex");
    symlink(&target, &link).unwrap();
    let path_var = std::env::join_paths([bin]).unwrap();
    assert_eq!(path_cli_in(&path_var), Some(link));
}

#[cfg(unix)]
#[test]
fn nvm_layouts_are_bounded_version_sorted_and_keep_the_launcher_path() {
    use std::os::unix::fs::symlink;
    let dir = private_dir();
    let nvm = dir.path().join(".config/nvm");
    for version in ["v1.0.0", "v2.10.0", "v2.9.0"] {
        let install = nvm.join("versions/node").join(version);
        let bin = install.join("bin");
        let package = install.join("lib/node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        script(
            &bin,
            "node",
            "script=$1; shift; exec /bin/sh \"$script\" \"$@\"",
            0o755,
        );
        let cli = script(&package, "codex.js", "echo codex-cli 0.200.0", 0o755);
        std::fs::write(&cli, "#!/usr/bin/env node\necho codex-cli 0.200.0\n").unwrap();
        symlink(cli, bin.join("codex")).unwrap();
    }
    let roots = nvm_roots(dir.path(), None, None);
    assert!(roots.contains(&nvm));
    let installations = nvm_installations(&nvm, RuntimePlatform::current().unwrap());
    let unrelated = dir.path().join("projects/nested/bin");
    std::fs::create_dir_all(&unrelated).unwrap();
    let unrelated_cli = script(&unrelated, "codex", "echo codex-cli 0.156.1", 0o755);
    let path_bin = dir.path().join("path-bin");
    std::fs::create_dir(&path_bin).unwrap();
    let path_cli = script(&path_bin, "codex", "echo codex-cli 0.158.0", 0o755);
    let codex_home = dir.path().join("codex-home");
    std::fs::create_dir(&codex_home).unwrap();
    let path_var = std::env::join_paths([&path_bin]).unwrap();
    let discovered = discover(&codex_home, dir.path(), None, Some(&path_var), &roots);
    let discovered_nvm = discovered
        .iter()
        .filter(|candidate| candidate.source == "nvm")
        .map(|candidate| candidate.path.clone())
        .collect::<Vec<_>>();
    let path_index = discovered
        .iter()
        .position(|candidate| candidate.source == "path_cli")
        .unwrap();
    let nvm_index = discovered
        .iter()
        .position(|candidate| candidate.source == "nvm")
        .unwrap();
    assert!(
        path_index < nvm_index,
        "PATH candidate precedes nvm fallback"
    );
    assert_eq!(discovered[path_index].path, path_cli);
    let path_observed = crate::runtime_inventory::version::observe(&path_cli);
    let nvm_observed = crate::runtime_inventory::version::observe(&installations[0]);
    assert_eq!(path_observed["status"], "supported");
    assert_eq!(path_observed["installed"], "0.158.0");
    assert_eq!(nvm_observed["status"], "supported");
    assert_eq!(nvm_observed["installed"], "0.200.0");
    assert_eq!(
        installations
            .iter()
            .map(|path| {
                path.parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
            })
            .collect::<Vec<_>>(),
        vec!["v2.10.0", "v2.9.0", "v1.0.0"]
    );
    assert_eq!(discovered_nvm, installations);
    assert!(
        discovered
            .iter()
            .all(|candidate| candidate.path != unrelated_cli)
    );
    assert!(installations[0].is_symlink());

    let alias_bin = dir.path().join("alias-bin");
    std::fs::create_dir(&alias_bin).unwrap();
    symlink(&installations[0], alias_bin.join("codex")).unwrap();
    let alias_path = std::env::join_paths([&alias_bin]).unwrap();
    let aliased = discover(&codex_home, dir.path(), None, Some(&alias_path), &roots);
    let target = installations[0].canonicalize().unwrap();
    let aliases = aliased
        .iter()
        .filter(|candidate| candidate.path.canonicalize().ok().as_ref() == Some(&target))
        .collect::<Vec<_>>();
    assert_eq!(aliases.len(), 1);
    assert_eq!(aliases[0].source, "path_cli");
}

#[cfg(unix)]
#[test]
fn nvm_roots_use_only_absolute_known_locations() {
    use std::ffi::OsStr;
    let dir = private_dir();
    let home = dir.path().join("home");
    let custom_nvm = dir.path().join("custom-nvm");
    let config_home = home.join(".config");
    assert_eq!(
        nvm_roots(&home, Some(OsStr::new("relative/nvm")), None),
        vec![home.join(".config/nvm"), home.join(".nvm")]
    );
    assert_eq!(
        nvm_roots(
            &home,
            Some(custom_nvm.as_os_str()),
            Some(config_home.as_os_str()),
        ),
        vec![custom_nvm, config_home.join("nvm"), home.join(".nvm"),]
    );
}
