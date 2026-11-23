#[cfg(any(windows, test))]
mod cache;
mod metadata;
mod paths;
mod platform;
mod windows;

#[cfg(test)]
#[path = "discovery/matrix_tests.rs"]
mod matrix_tests;
#[cfg(all(test, unix))]
#[path = "discovery/tests.rs"]
mod tests;

#[cfg(test)]
use paths::app;
pub(super) use paths::nvm_roots;
#[cfg(all(test, unix))]
pub(super) use paths::path_cli_in;
use paths::{apps, cli_command_names, editor, executable, nvm_installations, path_cli_candidates};
use platform::{RuntimeOs, RuntimePlatform};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use windows::cli_runtime_for_launcher;
#[cfg(windows)]
use windows::registered_windows_codex_package_root;

pub(super) struct Candidate {
    pub(super) source: &'static str,
    pub(super) name: &'static str,
    pub(super) path: PathBuf,
    pub(super) unavailable: bool,
    pub(super) running: bool,
    pub(super) host_version: Option<String>,
    pub(super) fallbacks: Vec<PathBuf>,
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
    let packages = platform
        .filter(|platform| platform.os == RuntimeOs::Windows)
        .map(desktop_package_roots)
        .unwrap_or_default();
    let package_roots = packages
        .iter()
        .map(|(root, _)| root.clone())
        .collect::<Vec<_>>();
    let mut candidates = discover_with_platform(
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
    );
    let installed_paths = candidates
        .iter()
        .map(|candidate| candidate.path.canonicalize().ok())
        .collect::<Vec<_>>();
    let installed = installed_paths
        .iter()
        .filter_map(Option::as_deref)
        .collect::<BTreeSet<_>>();
    let mut processes = System::new();
    processes.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
    let running = processes
        .processes()
        .values()
        .filter_map(|process| {
            let path = process.exe()?;
            let name = path.file_name()?;
            if !(name.eq_ignore_ascii_case("codex") || name.eq_ignore_ascii_case("codex.exe")) {
                return None;
            }
            (installed.contains(path)
                || process.cmd().iter().skip(1).any(|arg| arg == "app-server"))
            .then(|| path.canonicalize().ok())
            .flatten()
        })
        .collect::<BTreeSet<_>>();
    for (candidate, path) in candidates.iter_mut().zip(&installed_paths) {
        candidate.running = path.as_ref().is_some_and(|path| running.contains(path));
        if candidate.source == "codex_app"
            && let Some((_, version)) = packages.iter().find(|(root, _)| {
                root.canonicalize()
                    .is_ok_and(|root| candidate.path.starts_with(root))
            })
        {
            candidate.host_version = Some(version.clone());
            #[cfg(windows)]
            if let Some(local) = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
            {
                candidate.fallbacks =
                    cache::verified_candidates(&candidate.path, &local.join("OpenAI/Codex/bin"));
            }
        }
    }
    candidates.extend(
        running
            .into_iter()
            .filter(|path| !installed.contains(path.as_path()))
            .map(|path| Candidate {
                source: "running",
                name: "Running Codex",
                path,
                unavailable: false,
                running: true,
                host_version: None,
                fallbacks: Vec::new(),
            }),
    );
    candidates
}

fn desktop_package_roots(platform: RuntimePlatform) -> Vec<(PathBuf, String)> {
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
                path: path.clone(),
                unavailable,
                running: false,
                host_version: metadata::host_version(source, &path),
                fallbacks: Vec::new(),
            });
        }
    };
    if let Some(configured) = configured {
        let path = platform
            .and_then(|platform| cli_runtime_for_launcher(configured, platform))
            .unwrap_or_else(|| configured.to_owned());
        let unavailable = matches!(path.try_exists(), Ok(false))
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
    if let Some(platform) = platform {
        for path in apps(home, user_home, platform, system_root, package_roots) {
            add("codex_app", "ChatGPT App (Codex)", path, false);
        }
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

    if let Some(platform) = platform {
        let desktop_cli_roots = (platform.os == RuntimeOs::Macos)
            .then_some(["opt/homebrew/bin", "usr/local/bin"])
            .into_iter()
            .flatten()
            .map(|path| system_root.join(path));
        let roots = path_var
            .into_iter()
            .flat_map(std::env::split_paths)
            .chain(desktop_cli_roots);
        let command_names = cli_command_names(platform, path_extensions);
        for path in path_cli_candidates(roots, &command_names, platform) {
            add("path_cli", "Local Codex CLI", path, false);
        }
        for root in nvm_roots {
            for path in nvm_installations(root, platform) {
                add("nvm", "nvm Node Codex CLI", path, false);
            }
        }
    }
    result
}

pub(super) fn verified_fallback(candidate: &Candidate, path: &Path) -> bool {
    #[cfg(any(windows, test))]
    {
        cache::matches_resource(&candidate.path, path)
    }
    #[cfg(not(any(windows, test)))]
    {
        let _ = (candidate, path);
        false
    }
}
