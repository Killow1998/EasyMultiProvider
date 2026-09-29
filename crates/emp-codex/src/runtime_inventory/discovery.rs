//! Known installation layouts only; no recursive search of user directories.
mod paths;
mod platform;
mod windows;

#[cfg(test)]
#[path = "discovery/matrix_tests.rs"]
mod matrix_tests;
#[cfg(all(test, unix))]
#[path = "discovery/tests.rs"]
mod tests;

use paths::{app, editor, executable, nvm_installations, path_cli_in_with_extensions};
pub(super) use paths::{nvm_roots, path_cli_in};
use platform::{RuntimeOs, RuntimePlatform};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use windows::cli_runtime_for_launcher;
#[cfg(windows)]
use windows::registered_windows_codex_package_root;

pub(super) struct Candidate {
    pub(super) source: &'static str,
    pub(super) name: &'static str,
    pub(super) path: PathBuf,
    pub(super) unavailable: bool,
}

pub(super) fn discover(
    home: &Path,
    user_home: &Path,
    configured: Option<&Path>,
    path_var: Option<&OsStr>,
    nvm_roots: &[PathBuf],
) -> Vec<Candidate> {
    let platform = RuntimePlatform::current();
    let path_extensions = std::env::var_os("PATHEXT");
    let package_roots = platform
        .filter(|platform| platform.os == RuntimeOs::Windows)
        .map(desktop_package_roots)
        .unwrap_or_default();
    discover_with_platform(
        home,
        user_home,
        configured,
        path_var,
        nvm_roots,
        DiscoveryEnvironment {
            platform,
            path_extensions: path_extensions.as_deref(),
            system_root: Path::new("/"),
            package_roots: &package_roots,
        },
    )
}

fn desktop_package_roots(platform: RuntimePlatform) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        registered_windows_codex_package_root(platform)
            .into_iter()
            .collect()
    }
    #[cfg(not(windows))]
    {
        let _ = platform;
        Vec::new()
    }
}

#[derive(Clone, Copy)]
struct DiscoveryEnvironment<'a> {
    platform: Option<RuntimePlatform>,
    path_extensions: Option<&'a OsStr>,
    system_root: &'a Path,
    package_roots: &'a [PathBuf],
}

fn discover_with_platform(
    home: &Path,
    user_home: &Path,
    configured: Option<&Path>,
    path_var: Option<&OsStr>,
    nvm_roots: &[PathBuf],
    environment: DiscoveryEnvironment<'_>,
) -> Vec<Candidate> {
    let DiscoveryEnvironment {
        platform,
        path_extensions,
        system_root,
        package_roots,
    } = environment;
    let mut result = Vec::new();
    let mut add = |source, name, path: PathBuf, unavailable: bool| {
        let canonical = path
            .canonicalize()
            .unwrap_or_else(|_| emp_state::config::resolve_user_path(&path));
        if !result.iter().any(|candidate: &Candidate| {
            candidate
                .path
                .canonicalize()
                .unwrap_or_else(|_| emp_state::config::resolve_user_path(&candidate.path))
                == canonical
        }) {
            result.push(Candidate {
                source,
                name,
                path,
                unavailable,
            });
        }
    };
    if let Some(configured) = configured {
        let path = platform
            .and_then(|platform| cli_runtime_for_launcher(configured, platform))
            .unwrap_or_else(|| configured.to_owned());
        let unavailable = !path.exists()
            || platform.is_some_and(|platform| {
                platform.os == RuntimeOs::Windows && path == configured && {
                    configured
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
                        || configured
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("bat"))
                        || configured
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("ps1"))
                }
            });
        add("configured", "Configured Codex", path, unavailable);
    }
    if let Some(platform) = platform
        && let Some(path) = app(home, user_home, platform, system_root, package_roots)
    {
        add("codex_app", "ChatGPT App (Codex)", path, false);
    }
    let binary_name = platform.map_or_else(
        || if cfg!(windows) { "codex.exe" } else { "codex" },
        RuntimePlatform::executable_name,
    );
    let root = home.join("packages/standalone/current");
    if let Some(path) = [root.join("bin"), root]
        .into_iter()
        .map(|root| root.join(binary_name))
        .find(|path| executable(path))
    {
        add("managed", "Managed Codex runtime", path, false);
    }
    let daemon_binary = home
        .join("packages/app-server-daemon/current/bin")
        .join(binary_name);
    if executable(&daemon_binary) {
        add(
            "app_server_daemon",
            "App server daemon Codex",
            daemon_binary,
            false,
        );
    }
    if let Some(platform) = platform {
        for (source, name, folder) in [
            ("vscode", "VS Code extension", ".vscode"),
            (
                "vscode_insiders",
                "VS Code Insiders extension",
                ".vscode-insiders",
            ),
            ("cursor", "Cursor extension", ".cursor"),
        ] {
            if let Some(path) = editor(&user_home.join(folder).join("extensions"), platform) {
                add(source, name, path, false);
            }
        }
    }

    if let Some(path) = path_var.and_then(|value| {
        platform.and_then(|platform| path_cli_in_with_extensions(value, platform, path_extensions))
    }) {
        add("path_cli", "PATH Codex CLI", path, false);
    }
    if let Some(platform) = platform {
        for root in nvm_roots {
            for path in nvm_installations(root, platform) {
                add("nvm", "nvm Node Codex CLI", path, false);
            }
        }
    }
    result
}
