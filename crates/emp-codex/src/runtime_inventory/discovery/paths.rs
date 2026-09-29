//! Known on-disk installation layouts and trust-gated path lookup.
use super::platform::{RuntimeOs, RuntimePlatform};
use super::windows::cli_runtime_candidate_for_launcher;
use std::cmp::Ordering;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_NODE_INSTALLATIONS: usize = 64;

pub(super) fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
pub(super) fn runtime_file(path: &Path) -> Option<PathBuf> {
    if path.is_symlink()
        || !path.is_file()
        || !(path
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("exe"))
            || executable(path))
    {
        return None;
    }
    path.canonicalize().ok()
}
/// Search `path_var` for a trusted `codex`. Empty and relative entries are
/// skipped so a PATH such as `:/usr/bin` or `.:bin` never resolves against the
/// current working directory.
pub(in crate::runtime_inventory) fn path_cli_in(path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    let platform = RuntimePlatform::current()?;
    path_cli_in_for(path_var, platform)
}

pub(super) fn path_cli_in_for(path_var: &OsStr, platform: RuntimePlatform) -> Option<PathBuf> {
    let path_extensions = std::env::var_os("PATHEXT");
    path_cli_in_with_extensions(path_var, platform, path_extensions.as_deref())
}

pub(super) fn path_cli_in_with_extensions(
    path_var: &OsStr,
    platform: RuntimePlatform,
    path_extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    let names = cli_command_names(platform, path_extensions);
    path_cli_in_roots(std::env::split_paths(path_var), &names, platform)
}

pub(super) fn cli_command_names(
    platform: RuntimePlatform,
    path_extensions: Option<&OsStr>,
) -> Vec<String> {
    if platform.os == RuntimeOs::Windows {
        let extensions = path_extensions
            .map(OsStr::to_string_lossy)
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
        extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| format!("codex{}", extension.to_ascii_lowercase()))
            .collect()
    } else {
        vec!["codex".to_owned()]
    }
}

pub(super) fn path_cli_in_roots(
    roots: impl IntoIterator<Item = PathBuf>,
    names: &[String],
    platform: RuntimePlatform,
) -> Option<PathBuf> {
    path_cli_in_roots_with_trust(roots, names, platform, super::super::trust::trusted_binary)
}

pub(super) fn path_cli_in_roots_with_trust(
    roots: impl IntoIterator<Item = PathBuf>,
    names: &[String],
    platform: RuntimePlatform,
    mut trusted_path: impl FnMut(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    roots
        .into_iter()
        .filter(|root| !root.as_os_str().is_empty() && root.is_absolute())
        .find_map(|root| {
            names.iter().find_map(|name| {
                let launcher = root.join(name);
                if !executable(&launcher) {
                    return None;
                }
                let candidate = cli_runtime_candidate_for_launcher(&launcher, platform)?;
                let trusted = trusted_path(&candidate)?;
                // Unix npm launchers are often symlinks; preserve their parent
                // so version probing can locate the sibling Node executable.
                Some(if platform.os == RuntimeOs::Windows {
                    trusted
                } else {
                    candidate
                })
            })
        })
}
fn newest(paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    paths
        .filter_map(|path| {
            let time = path
                .metadata()
                .ok()?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            Some((time, path))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)))
        .map(|(_, path)| path)
}
fn entries(root: &Path) -> Option<Vec<PathBuf>> {
    root.read_dir()
        .ok()?
        .map(|entry| entry.ok().map(|entry| entry.path()))
        .collect()
}
pub(super) fn editor(root: &Path, platform: RuntimePlatform) -> Option<PathBuf> {
    let extensions = entries(root)?
        .into_iter()
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .to_lowercase()
                    .starts_with("openai.chatgpt-")
            })
        })
        .collect::<Vec<_>>();
    if extensions.len() > 64 {
        return None;
    }
    let canonical = root.canonicalize().ok()?;
    newest(
        extensions
            .into_iter()
            .map(|extension| {
                extension
                    .join("bin")
                    .join(platform.editor_bin_directory())
                    .join(platform.executable_name())
            })
            .filter_map(|path| runtime_file(&path))
            .filter(|path| path.starts_with(&canonical)),
    )
}
fn desktop_app_candidates(
    home: &Path,
    user_home: &Path,
    platform: RuntimePlatform,
    system_root: &Path,
    package_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    match platform.os {
        RuntimeOs::Windows => {
            for root in package_roots {
                paths.push(root.join("app/resources/codex.exe"));
                paths.push(root.join("app/resources/codex"));
            }
        }
        RuntimeOs::Linux => {
            paths.push(system_root.join("usr/lib/chatgpt/resources/codex"));
        }
        RuntimeOs::Macos => {
            for root in [
                user_home.join("Applications"),
                system_root.join("Applications"),
            ] {
                for name in ["ChatGPT.app", "Codex.app"] {
                    paths.push(root.join(name).join("Contents/Resources/codex"));
                }
            }
        }
    }
    paths.push(
        home.join("plugins/.plugin-appserver")
            .join(platform.executable_name()),
    );
    paths
}

pub(super) fn app(
    home: &Path,
    user_home: &Path,
    platform: RuntimePlatform,
    system_root: &Path,
    package_roots: &[PathBuf],
) -> Option<PathBuf> {
    desktop_app_candidates(home, user_home, platform, system_root, package_roots)
        .into_iter()
        .find_map(|path| runtime_file(&path))
}
/// Known nvm roots only. This reads their direct Node-version children and
/// never sources shell startup files or walks the user's home recursively.
pub(in crate::runtime_inventory) fn nvm_roots(
    user_home: &Path,
    nvm_dir: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let _ = (user_home, nvm_dir, xdg_config_home);
        Vec::new()
    }
    #[cfg(not(windows))]
    {
        let mut roots = Vec::new();
        let mut add = |root: PathBuf| {
            if root.is_absolute() && !roots.contains(&root) {
                roots.push(root);
            }
        };
        if let Some(root) = nvm_dir.map(PathBuf::from) {
            add(root);
        }
        if let Some(root) = xdg_config_home.map(PathBuf::from) {
            add(root.join("nvm"));
        }
        if user_home.is_absolute() {
            add(user_home.join(".config/nvm"));
            add(user_home.join(".nvm"));
        }
        roots
    }
}

fn node_version(path: &Path) -> Option<(u64, u64, u64, bool)> {
    let name = path.file_name()?.to_str()?.strip_prefix('v')?;
    let stable = !name.contains('-');
    let core = name.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        stable,
    ))
}

fn compare_node_versions(left: &Path, right: &Path) -> Ordering {
    let left_version = node_version(left);
    let right_version = node_version(right);
    match (left_version, right_version) {
        (Some(left_version), Some(right_version)) => right_version
            .cmp(&left_version)
            .then_with(|| right.file_name().cmp(&left.file_name())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => right.file_name().cmp(&left.file_name()),
    }
}

pub(super) fn nvm_installations(root: &Path, platform: RuntimePlatform) -> Vec<PathBuf> {
    let Some(mut installations) = entries(&root.join("versions/node")) else {
        return Vec::new();
    };
    installations.retain(|path| path.is_dir());
    installations.sort_by(|left, right| compare_node_versions(left, right));
    installations.truncate(MAX_NODE_INSTALLATIONS);
    installations
        .into_iter()
        .map(|node| node.join("bin").join(platform.executable_name()))
        .filter(|path| executable(path))
        .collect()
}
