//! Cross-platform source layout and package selection fixtures.
use super::paths;
use super::platform::RuntimeArch;
use super::windows::{windows_package_list_response_is_stable, windows_package_name_matches_arch};
use super::{
    DiscoveryEnvironment, RuntimeOs, RuntimePlatform, app, discover_with_platform, editor,
};
use crate::runtime_inventory::choose_helper;
use serde_json::json;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Target {
    name: &'static str,
    platform: RuntimePlatform,
    editor_directory: &'static str,
    executable: &'static str,
    cli_triple: &'static str,
    cli_package: &'static str,
}

fn matrix() -> [Target; 4] {
    [
        Target {
            name: "Windows x64",
            platform: RuntimePlatform {
                os: RuntimeOs::Windows,
                arch: RuntimeArch::X64,
            },
            editor_directory: "windows-x86_64",
            executable: "codex.exe",
            cli_triple: "x86_64-pc-windows-msvc",
            cli_package: "codex-win32-x64",
        },
        Target {
            name: "Linux x64",
            platform: RuntimePlatform {
                os: RuntimeOs::Linux,
                arch: RuntimeArch::X64,
            },
            editor_directory: "linux-x86_64",
            executable: "codex",
            cli_triple: "x86_64-unknown-linux-musl",
            cli_package: "codex-linux-x64",
        },
        Target {
            name: "macOS Intel",
            platform: RuntimePlatform {
                os: RuntimeOs::Macos,
                arch: RuntimeArch::X64,
            },
            editor_directory: "macos-x86_64",
            executable: "codex",
            cli_triple: "x86_64-apple-darwin",
            cli_package: "codex-darwin-x64",
        },
        Target {
            name: "macOS Apple Silicon",
            platform: RuntimePlatform {
                os: RuntimeOs::Macos,
                arch: RuntimeArch::Arm64,
            },
            editor_directory: "macos-aarch64",
            executable: "codex",
            cli_triple: "aarch64-apple-darwin",
            cli_package: "codex-darwin-arm64",
        },
    ]
}

fn fixture_root() -> TempDir {
    #[cfg(unix)]
    {
        crate::runtime_inventory::trust::tests::private_dir()
    }
    #[cfg(not(unix))]
    {
        tempfile::Builder::new()
            .prefix("emp-runtime-matrix-")
            .tempdir()
            .expect("create inert runtime fixture directory")
    }
}

/// Write inert data at an executable-looking path. Tests inspect metadata
/// only and never invoke these files.
fn inert_file(path: &Path) -> PathBuf {
    inert_file_with_contents(path, b"inert runtime fixture; never execute")
}

fn inert_file_with_contents(path: &Path, contents: &[u8]) -> PathBuf {
    fs::create_dir_all(path.parent().expect("fixture parent")).unwrap();
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path.to_owned()
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap()
}

#[test]
fn runtime_source_paths_cover_the_four_by_three_installation_matrix() {
    let all_editor_directories = [
        "windows-x86_64",
        "linux-x86_64",
        "macos-x86_64",
        "macos-aarch64",
    ];

    for target in matrix() {
        let fixture = fixture_root();
        let root = fixture.path();

        // VS Code VSIX: populate every platform directory as inert data,
        // then verify discovery returns exactly the requested OS/arch.
        let extensions = root.join("user/.vscode/extensions");
        let extension = extensions.join("openai.chatgpt-matrix");
        for directory in all_editor_directories {
            let name = if directory.starts_with("windows-") {
                "codex.exe"
            } else {
                "codex"
            };
            inert_file(&extension.join("bin").join(directory).join(name));
        }
        // The Windows VSIX also carries a Linux WSL runtime. It is never
        // a Windows-native candidate.
        inert_file(&extension.join("bin/linux-x86_64/codex"));
        let expected_editor = extension
            .join("bin")
            .join(target.editor_directory)
            .join(target.executable);
        assert_eq!(
            editor(&extensions, target.platform).as_deref(),
            Some(canonical(&expected_editor).as_path()),
            "{} VS Code candidate",
            target.name
        );
        fs::remove_file(&expected_editor).unwrap();
        assert_eq!(
            editor(&extensions, target.platform),
            None,
            "{} VS Code candidate must be absent when its native file is absent",
            target.name
        );

        // Codex CLI: the app-server daemon is in CODEX_HOME's documented
        // package directory. PATH uses the native npm payload on Windows,
        // without running its .cmd shim or requiring node.exe.
        let codex_home = root.join("codex-home");
        let daemon = inert_file(
            &codex_home
                .join("packages/app-server-daemon/current/bin")
                .join(target.executable),
        );
        let path_root = root.join("npm-prefix");
        let (launcher, native_cli, hoisted_cli, bundled_cli) = if target.platform.os
            == RuntimeOs::Windows
        {
            let launcher = inert_file(&path_root.join("codex.cmd"));
            let hoisted = inert_file_with_contents(
                &path_root
                    .join("node_modules/@openai")
                    .join(target.cli_package)
                    .join("vendor")
                    .join(target.cli_triple)
                    .join("bin/codex.exe"),
                b"codex-cli 0.157.0; hoisted payload; never execute",
            );
            let nested = inert_file_with_contents(
                &path_root
                    .join("node_modules/@openai/codex/node_modules/@openai")
                    .join(target.cli_package)
                    .join("vendor")
                    .join(target.cli_triple)
                    .join("bin/codex.exe"),
                b"codex-cli 0.158.0; nested payload; never execute",
            );
            let bundled = inert_file_with_contents(
                &path_root
                    .join("node_modules/@openai/codex/vendor")
                    .join(target.cli_triple)
                    .join("bin/codex.exe"),
                b"codex-cli 0.156.9; package vendor fallback; never execute",
            );
            // A wrong-architecture optional package must not win.
            inert_file_with_contents(
                &path_root.join("node_modules/@openai/codex-win32-arm64/vendor/aarch64-pc-windows-msvc/bin/codex.exe"),
                b"codex-cli 0.199.0; wrong architecture; never execute",
            );
            assert!(!path_root.join("node.exe").exists());
            (launcher, nested, Some(hoisted), Some(bundled))
        } else {
            let launcher = inert_file(&path_root.join("codex"));
            (launcher.clone(), launcher, None, None)
        };
        let path_var = std::env::join_paths([&path_root]).unwrap();
        let extensions =
            (target.platform.os == RuntimeOs::Windows).then_some(OsStr::new(".COM;.EXE;.BAT;.CMD"));
        let user_home = root.join("user");
        let system_root = root.join("system");
        assert!(
            paths::executable(&launcher),
            "{} launcher fixture is not executable: {}",
            target.name,
            launcher.display()
        );
        assert!(
            paths::executable(&native_cli),
            "{} native fixture is not executable: {}",
            target.name,
            native_cli.display()
        );
        assert_eq!(target.platform.cli_target_triple(), target.cli_triple);
        assert_eq!(target.platform.cli_optional_package(), target.cli_package);
        let names = paths::cli_command_names(target.platform, extensions);
        let expected_path_cli = match target.platform.os {
            RuntimeOs::Windows => canonical(&native_cli),
            RuntimeOs::Linux | RuntimeOs::Macos => launcher.clone(),
        };
        assert_eq!(
            paths::path_cli_in_roots_with_trust(
                [path_root.clone()],
                &names,
                target.platform,
                |candidate| candidate.canonicalize().ok(),
            )
            .as_deref(),
            Some(expected_path_cli.as_path()),
            "{} PATH CLI candidate",
            target.name
        );
        if target.platform.os == RuntimeOs::Windows {
            assert_ne!(canonical(&native_cli), canonical(&launcher));
            assert_eq!(native_cli.file_name(), Some(OsStr::new("codex.exe")));
        }
        let candidates = discover_with_platform(
            &codex_home,
            &user_home,
            None,
            Some(&path_var),
            &[],
            DiscoveryEnvironment {
                platform: Some(target.platform),
                path_extensions: extensions,
                system_root: &system_root,
                package_roots: &[],
            },
        );
        assert!(
            candidates.iter().any(
                |candidate| candidate.source == "app_server_daemon" && candidate.path == daemon
            ),
            "{} app-server daemon candidate",
            target.name
        );
        if let (Some(hoisted_cli), Some(bundled_cli)) = (&hoisted_cli, &bundled_cli) {
            assert_eq!(
                fs::read(&native_cli).unwrap(),
                b"codex-cli 0.158.0; nested payload; never execute"
            );
            fs::remove_file(&native_cli).unwrap();
            assert_eq!(
                paths::path_cli_in_roots_with_trust(
                    [path_root.clone()],
                    &names,
                    target.platform,
                    |candidate| candidate.canonicalize().ok(),
                )
                .as_deref(),
                Some(canonical(hoisted_cli).as_path()),
                "{} hoisted optional package is the second resolution",
                target.name
            );
            assert_eq!(
                fs::read(hoisted_cli).unwrap(),
                b"codex-cli 0.157.0; hoisted payload; never execute"
            );
            fs::remove_file(hoisted_cli).unwrap();
            assert_eq!(
                paths::path_cli_in_roots_with_trust(
                    [path_root.clone()],
                    &names,
                    target.platform,
                    |candidate| candidate.canonicalize().ok(),
                )
                .as_deref(),
                Some(canonical(bundled_cli).as_path()),
                "{} package vendor fallback",
                target.name
            );
            assert_eq!(
                fs::read(bundled_cli).unwrap(),
                b"codex-cli 0.156.9; package vendor fallback; never execute"
            );
            fs::remove_file(bundled_cli).unwrap();
        } else {
            fs::remove_file(&native_cli).unwrap();
        }
        assert_eq!(
            paths::path_cli_in_roots_with_trust(
                [path_root.clone()],
                &names,
                target.platform,
                |candidate| candidate.canonicalize().ok(),
            ),
            None,
            "{} PATH CLI candidate must be absent when native payload is absent",
            target.name
        );

        // ChatGPT desktop app: use the native Codex runtime resource and
        // ignore platform launchers or GUI executables.
        let (package_roots, desktop_expected, desktop_decoy) = match target.platform.os {
            RuntimeOs::Windows => {
                let package_root = root.join("registered-package");
                let gui = inert_file(&package_root.join("app/Codex.exe"));
                let extensionless = inert_file(&package_root.join("app/resources/codex"));
                let runtime = inert_file(&package_root.join("app/resources/codex.exe"));
                (vec![package_root], runtime, vec![gui, extensionless])
            }
            RuntimeOs::Linux => {
                let launcher = inert_file(&system_root.join("usr/lib/chatgpt/codex-launcher"));
                let runtime = inert_file(&system_root.join("usr/lib/chatgpt/resources/codex"));
                (Vec::new(), runtime, vec![launcher])
            }
            RuntimeOs::Macos => {
                let chatgpt = inert_file(
                    &user_home.join("Applications/ChatGPT.app/Contents/Resources/codex"),
                );
                let codex = inert_file(
                    &system_root.join("Applications/Codex.app/Contents/Resources/codex"),
                );
                (Vec::new(), chatgpt, vec![codex])
            }
        };
        assert_eq!(
            app(
                &codex_home,
                &user_home,
                target.platform,
                &system_root,
                &package_roots,
            )
            .as_deref(),
            Some(canonical(&desktop_expected).as_path()),
            "{} desktop app candidate",
            target.name
        );
        if target.platform.os == RuntimeOs::Windows {
            let package_root = &package_roots[0];
            fs::remove_file(&desktop_expected).unwrap();
            assert_eq!(
                app(
                    &codex_home,
                    &user_home,
                    target.platform,
                    &system_root,
                    &package_roots,
                ),
                Some(canonical(&desktop_decoy[1])),
                "Windows prefers resources/codex.exe, then resources/codex"
            );
            fs::remove_file(&desktop_decoy[1]).unwrap();
            assert!(package_root.join("app/Codex.exe").is_file());
        } else if target.platform.os == RuntimeOs::Macos {
            fs::remove_file(&desktop_expected).unwrap();
            assert_eq!(
                app(
                    &codex_home,
                    &user_home,
                    target.platform,
                    &system_root,
                    &package_roots,
                ),
                Some(canonical(&desktop_decoy[0])),
                "macOS system Codex.app fallback"
            );
            fs::remove_file(&desktop_decoy[0]).unwrap();
        } else {
            fs::remove_file(&desktop_expected).unwrap();
        }
        assert_eq!(
            app(
                &codex_home,
                &user_home,
                target.platform,
                &system_root,
                &package_roots,
            ),
            None,
            "{} desktop candidate must be absent after native runtime removal",
            target.name
        );
    }
}

#[test]
fn helper_selection_prefers_configured_available_then_first_available() {
    let unavailable = json!({"source": "configured", "available": false});
    let configured = json!({"source": "configured", "available": true});
    let first_available = json!({"source": "app_server_daemon", "available": true});
    assert_eq!(choose_helper(&[]), None);
    assert_eq!(choose_helper(&[unavailable]), None);
    assert_eq!(
        choose_helper(&[
            json!({"source": "configured", "available": false}),
            first_available.clone(),
        ]),
        Some(1)
    );
    assert_eq!(
        choose_helper(&[first_available, configured]),
        Some(1),
        "configured available runtime retains precedence"
    );
}

#[test]
fn windows_local_npm_bin_resolves_adjacent_codex_package() {
    let fixture = fixture_root();
    let root = fixture.path().join("project/node_modules");
    let bin_directory = root.join(".bin");
    let launcher = inert_file(&bin_directory.join("codex.cmd"));
    let platform = RuntimePlatform {
        os: RuntimeOs::Windows,
        arch: RuntimeArch::X64,
    };
    let nested = inert_file_with_contents(
        &root.join("@openai/codex/node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe"),
        b"codex-cli 0.158.0; nested local payload; never execute",
    );
    let hoisted = inert_file_with_contents(
        &root.join("@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin/codex.exe"),
        b"codex-cli 0.157.0; hoisted local payload; never execute",
    );
    let bundled = inert_file_with_contents(
        &root.join("@openai/codex/vendor/x86_64-pc-windows-msvc/bin/codex.exe"),
        b"codex-cli 0.156.9; local package vendor fallback; never execute",
    );
    let names = ["codex.cmd".to_owned()];
    let resolve = || {
        paths::path_cli_in_roots_with_trust(
            [bin_directory.clone()],
            &names,
            platform,
            |candidate| candidate.canonicalize().ok(),
        )
    };

    assert!(paths::executable(&launcher));
    assert_eq!(resolve().as_deref(), Some(canonical(&nested).as_path()));
    assert_eq!(
        fs::read(&nested).unwrap(),
        b"codex-cli 0.158.0; nested local payload; never execute"
    );
    fs::remove_file(&nested).unwrap();
    assert_eq!(resolve().as_deref(), Some(canonical(&hoisted).as_path()));
    assert_eq!(
        fs::read(&hoisted).unwrap(),
        b"codex-cli 0.157.0; hoisted local payload; never execute"
    );
    fs::remove_file(&hoisted).unwrap();
    assert_eq!(resolve().as_deref(), Some(canonical(&bundled).as_path()));
    assert_eq!(
        fs::read(&bundled).unwrap(),
        b"codex-cli 0.156.9; local package vendor fallback; never execute"
    );
    assert!(!fixture.path().join("project/node.exe").exists());
}

#[test]
fn absent_explicit_configuration_remains_visible_as_unavailable() {
    for target in matrix() {
        let fixture = fixture_root();
        let root = fixture.path();
        let name = if target.platform.os == RuntimeOs::Windows {
            "codex.cmd"
        } else {
            "codex"
        };
        let configured = root.join("configured-prefix").join(name);
        assert!(!configured.exists());
        let candidates = discover_with_platform(
            &root.join("codex-home"),
            &root.join("user-home"),
            Some(&configured),
            None,
            &[],
            DiscoveryEnvironment {
                platform: Some(target.platform),
                path_extensions: Some(OsStr::new(".COM;.EXE;.BAT;.CMD")),
                system_root: &root.join("system"),
                package_roots: &[],
            },
        );
        let configured_candidate = candidates
            .iter()
            .find(|candidate| candidate.source == "configured")
            .expect("explicitly configured Codex remains listed");
        assert_eq!(configured_candidate.path, configured);
        assert!(configured_candidate.unavailable);
    }
}

#[test]
fn windows_package_full_name_architecture_is_checked_exactly() {
    let x64 = "OpenAI.Codex_26.924.2738.0_x64__2p2nqsd0c76g0";
    let arm64 = "OpenAI.Codex_26.924.2738.0_arm64__2p2nqsd0c76g0";
    assert!(windows_package_name_matches_arch(x64, "x64"));
    assert!(!windows_package_name_matches_arch(x64, "arm64"));
    assert!(windows_package_name_matches_arch(arm64, "arm64"));
    assert!(!windows_package_name_matches_arch(arm64, "x64"));
    assert!(!windows_package_name_matches_arch(
        "OpenAI.Other_26.924.2738.0_x64__2p2nqsd0c76g0",
        "x64"
    ));
    assert!(!windows_package_name_matches_arch(
        "OpenAI.Codex_26.924.2738.0_x64__wrongpublisher",
        "x64"
    ));
    assert!(!windows_package_name_matches_arch(
        "OpenAI.Codex_x64",
        "x64"
    ));
}

#[test]
fn windows_package_enumeration_rejects_second_call_size_changes() {
    assert!(windows_package_list_response_is_stable(1, 128, 1, 128));
    assert!(!windows_package_list_response_is_stable(2, 128, 1, 64));
    assert!(!windows_package_list_response_is_stable(1, 128, 1, 127));
    assert!(!windows_package_list_response_is_stable(1, 128, 1, 129));
}

#[test]
fn mac_intel_nested_chatgpt_engine_stays_inside_outer_bundle() {
    let fixture = fixture_root();
    let root = fixture.path();
    let target = matrix()
        .into_iter()
        .find(|t| t.name == "macOS Intel")
        .unwrap();
    let engine = root.join(
        "Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    );
    inert_file(&engine);
    assert_eq!(
        app(
            &root.join("home"),
            &root.join("user"),
            target.platform,
            root,
            &[]
        ),
        Some(canonical(&engine))
    );
    #[cfg(unix)]
    {
        fs::remove_file(&engine).unwrap();
        let outside = inert_file(&root.join("outside/codex"));
        // A symlinked parent is canonicalized, then rejected at the bundle boundary.
        fs::remove_dir_all(engine.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.parent().unwrap(), engine.parent().unwrap()).unwrap();
        assert!(
            app(
                &root.join("home"),
                &root.join("user"),
                target.platform,
                root,
                &[]
            )
            .is_none()
        );
    }
}
