use super::{
    DiscoveryEnvironment, TargetArch, TargetOs, TargetPlatform, is_node_launcher, resolve_with,
};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Clone, Copy)]
struct TargetCase {
    name: &'static str,
    platform: TargetPlatform,
}

fn targets() -> [TargetCase; 4] {
    [
        TargetCase {
            name: "Windows x64",
            platform: TargetPlatform {
                os: TargetOs::Windows,
                arch: TargetArch::X64,
            },
        },
        TargetCase {
            name: "Linux x64",
            platform: TargetPlatform {
                os: TargetOs::Linux,
                arch: TargetArch::X64,
            },
        },
        TargetCase {
            name: "macOS Intel",
            platform: TargetPlatform {
                os: TargetOs::Macos,
                arch: TargetArch::X64,
            },
        },
        TargetCase {
            name: "macOS Apple Silicon",
            platform: TargetPlatform {
                os: TargetOs::Macos,
                arch: TargetArch::Arm64,
            },
        },
    ]
}

fn fixture_root() -> TempDir {
    let root = tempfile::Builder::new()
        .prefix("emp-claude-discovery-")
        .tempdir()
        .expect("create discovery fixture root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    root
}

fn inert_executable(path: &Path, contents: &[u8]) -> PathBuf {
    fs::create_dir_all(path.parent().expect("fixture parent")).unwrap();
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path.to_path_buf()
}

fn environment(platform: TargetPlatform, root: &Path) -> DiscoveryEnvironment {
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    DiscoveryEnvironment {
        platform,
        home: Some(home.clone()),
        user_profile: Some(home),
        path: Some(OsString::new()),
        nvm_dir: None,
        nvm_home: None,
        xdg_config_home: None,
        app_data: None,
        npm_prefix: None,
        homebrew_prefix: None,
    }
}

// The discovery fixtures inject file-only trust so temporary directory
// ownership and modes do not affect resolver tests. Production ancestor trust
// is exercised through resolve_claude_cli.
fn fixture_trust(path: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    let metadata = fs::metadata(&canonical).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 || mode & 0o6022 != 0 {
            return None;
        }
    }
    Some(canonical)
}

#[test]
fn finds_native_installation_with_minimal_path_for_supported_matrix() {
    for target in targets() {
        let root = fixture_root();
        let mut env = environment(target.platform, root.path());
        let home = env.user_profile.as_ref().unwrap();
        let name = if target.platform.os == TargetOs::Windows {
            "claude.exe"
        } else {
            "claude"
        };
        let expected = inert_executable(
            &home.join(".local/bin").join(name),
            b"inert native Claude fixture; never execute",
        );
        env.path = Some(OsString::from("/usr/bin:/bin"));

        let cli = resolve_with(&env, fixture_trust).expect(target.name);
        assert_eq!(
            cli.executable,
            expected.canonicalize().unwrap(),
            "{}",
            target.name
        );
        assert_eq!(cli.child_path, env.path.unwrap(), "{}", target.name);
    }
}

#[test]
fn discovers_npm_native_payloads_in_known_prefixes_for_supported_matrix() {
    for target in targets() {
        let root = fixture_root();
        let mut env = environment(target.platform, root.path());
        let prefix = if target.platform.os == TargetOs::Windows {
            let app_data = root.path().join("AppData/Roaming");
            env.app_data = Some(app_data.clone());
            app_data.join("npm")
        } else {
            let prefix = root.path().join("npm-prefix");
            env.npm_prefix = Some(prefix.clone());
            prefix
        };
        let package_root = if target.platform.os == TargetOs::Windows {
            prefix.join("node_modules/@anthropic-ai/claude-code")
        } else {
            prefix.join("lib/node_modules/@anthropic-ai/claude-code")
        };
        let package = target.platform.npm_optional_package();
        let name = if target.platform.os == TargetOs::Windows {
            "claude.exe"
        } else {
            "claude"
        };
        let expected = inert_executable(
            &package_root
                .join("node_modules/@anthropic-ai")
                .join(package)
                .join(name),
            b"inert native npm payload; never execute",
        );
        env.path = Some(OsString::from("/usr/bin:/bin"));

        let cli = resolve_with(&env, fixture_trust).expect(target.name);
        assert_eq!(
            cli.executable,
            expected.canonicalize().unwrap(),
            "{}",
            target.name
        );
        assert_eq!(cli.child_path, env.path.unwrap(), "{}", target.name);
    }
}

#[test]
fn windows_cmd_shim_is_resolved_to_the_adjacent_native_npm_payload() {
    let target = targets()[0];
    let root = fixture_root();
    let bin = root.path().join("npm-prefix");
    let shim = inert_executable(&bin.join("claude.cmd"), b"not invoked by discovery");
    let package = inert_executable(
        &bin.join("node_modules/@anthropic-ai/claude-code/node_modules/@anthropic-ai/claude-code-win32-x64/claude.exe"),
        b"inert Windows native payload; never execute",
    );
    let mut env = environment(target.platform, root.path());
    env.path = Some(std::env::join_paths([bin]).unwrap());

    let cli = resolve_with(&env, fixture_trust).expect("recognized Windows npm payload");
    assert_eq!(cli.executable, package.canonicalize().unwrap());
    assert_ne!(cli.executable, shim.canonicalize().unwrap());
    assert_eq!(cli.child_path, env.path.unwrap());
}

#[test]
fn configured_homebrew_prefix_is_checked_without_a_path_entry() {
    let target = targets()[3];
    let root = fixture_root();
    let prefix = root.path().join("opt/homebrew");
    let expected = inert_executable(&prefix.join("bin/claude"), b"native Homebrew CLI");
    let mut env = environment(target.platform, root.path());
    env.homebrew_prefix = Some(prefix);

    let cli = resolve_with(&env, fixture_trust).expect("Homebrew Claude CLI");
    assert_eq!(cli.executable, expected.canonicalize().unwrap());
    assert_eq!(cli.child_path, env.path.unwrap());
}

#[test]
fn path_order_wins_and_unsafe_commands_are_skipped() {
    let target = targets()[1];
    let root = fixture_root();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let first_cli = inert_executable(&first.join("claude"), b"first");
    let second_cli = inert_executable(&second.join("claude"), b"second");
    let mut env = environment(target.platform, root.path());
    let fallback = inert_executable(
        &env.home.as_ref().unwrap().join(".local/bin/claude"),
        b"fallback native",
    );
    env.path = Some(std::env::join_paths([Path::new("relative"), &first, &second]).unwrap());

    let cli = resolve_with(&env, fixture_trust).expect("PATH candidate");
    assert_eq!(cli.executable, first_cli.canonicalize().unwrap());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&first_cli, fs::Permissions::from_mode(0o777)).unwrap();
    }
    let cli = resolve_with(&env, fixture_trust).expect("second safe PATH candidate");
    assert_eq!(cli.executable, second_cli.canonicalize().unwrap());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&second_cli, fs::Permissions::from_mode(0o777)).unwrap();
    }
    let cli = resolve_with(&env, fixture_trust).expect("trusted native fallback");
    assert_eq!(cli.executable, fallback.canonicalize().unwrap());
}

#[test]
fn missing_and_unsafe_candidates_do_not_resolve() {
    let target = targets()[1];
    let root = fixture_root();
    let bin = root.path().join("unsafe");
    let _command = inert_executable(&bin.join("claude"), b"unsafe");
    let mut env = environment(target.platform, root.path());
    env.path = Some(std::env::join_paths([&bin]).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&_command, fs::Permissions::from_mode(0o777)).unwrap();
    }
    assert!(resolve_with(&env, fixture_trust).is_none());

    env.path = Some(OsString::from("/missing/claude/path"));
    assert!(resolve_with(&env, fixture_trust).is_none());
}

#[test]
fn nvm_installation_is_found_without_adding_node_for_native_payload() {
    let target = targets()[1];
    let root = fixture_root();
    let nvm = root.path().join("nvm");
    let node_bin = nvm.join("versions/node/v22.5.0/bin");
    let expected = inert_executable(&node_bin.join("claude"), b"native executable");
    inert_executable(&node_bin.join("node"), b"node sibling fixture");
    let mut env = environment(target.platform, root.path());
    env.nvm_dir = Some(nvm);
    env.path = Some(OsString::from("/usr/bin:/bin"));

    let cli = resolve_with(&env, fixture_trust).expect("nvm-installed Claude");
    assert_eq!(cli.executable, expected.canonicalize().unwrap());
    assert_eq!(cli.child_path, env.path.unwrap());
}

#[test]
fn legacy_node_script_keeps_the_trusted_sibling_node_on_child_path() {
    use crate::runtime_inventory::launcher::PathAdjustment;

    let target = targets()[1];
    let root = fixture_root();
    let bin = root.path().join("nvm/versions/node/v20.20.2/bin");
    let launcher = inert_executable(&bin.join("claude"), b"#!/usr/bin/env node\n// old CLI");
    inert_executable(&bin.join("node"), b"node sibling fixture");
    let mut env = environment(target.platform, root.path());
    env.path = Some(OsString::from("/usr/bin:/bin"));
    env.nvm_dir = Some(root.path().join("nvm"));

    assert!(is_node_launcher(&launcher));
    let mut node_directory = None;
    let cli = super::resolve_with_path_adjustment(&env, fixture_trust, |directory, inherited| {
        node_directory = Some(directory.canonicalize().unwrap());
        let mut entries = vec![directory.canonicalize().unwrap()];
        if let Some(inherited) = inherited {
            entries.extend(std::env::split_paths(&inherited));
        }
        PathAdjustment::Set(std::env::join_paths(entries).unwrap())
    })
    .expect("legacy Node launcher");
    assert_eq!(cli.executable, launcher.canonicalize().unwrap());
    let child_entries = std::env::split_paths(&cli.child_path).collect::<Vec<_>>();
    assert_eq!(child_entries.first(), Some(&bin.canonicalize().unwrap()));
    assert_eq!(node_directory, Some(bin.canonicalize().unwrap()));
}
