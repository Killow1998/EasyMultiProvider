//! Known installation layouts only; no recursive search of user directories.
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub(super) struct Candidate {
    pub source: &'static str,
    pub name: &'static str,
    pub path: PathBuf,
}

fn executable(path: &Path) -> bool {
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
fn runtime_file(path: &Path) -> Option<PathBuf> {
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
pub(super) fn path_cli() -> Option<PathBuf> {
    path_cli_in(&std::env::var_os("PATH")?)
}
/// Search `path_var` for a trusted `codex`. Empty and relative entries are
/// skipped so a PATH such as `:/usr/bin` or `.:bin` never resolves against the
/// current working directory.
fn path_cli_in(path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    #[cfg(windows)]
    let names = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!("codex{ext}"))
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let names = ["codex".to_owned()];
    std::env::split_paths(path_var)
        .filter(|root| !root.as_os_str().is_empty() && root.is_absolute())
        .find_map(|root| {
            names
                .iter()
                .map(|name| root.join(name))
                .find(|path| executable(path) && super::trust::trusted_binary(path).is_some())
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
        .max()
        .map(|(_, path)| path)
}
fn entries(root: &Path) -> Option<Vec<PathBuf>> {
    root.read_dir()
        .ok()?
        .map(|entry| entry.ok().map(|entry| entry.path()))
        .collect()
}
fn editor(root: &Path) -> Option<PathBuf> {
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
    let system = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        return None;
    };
    let arch = std::env::consts::ARCH;
    if !matches!(arch, "x86_64" | "aarch64") {
        return None;
    }
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
    newest(
        extensions
            .into_iter()
            .flat_map(|extension| {
                let bin = extension.join("bin");
                [
                    bin.join(name),
                    bin.join(format!("{system}-{arch}")).join(name),
                ]
            })
            .filter_map(|path| runtime_file(&path))
            .filter(|path| path.starts_with(&canonical)),
    )
}
fn app(home: &Path, user_home: &Path) -> Option<PathBuf> {
    let plugin = home
        .join("plugins/.plugin-appserver")
        .join(if cfg!(windows) { "codex.exe" } else { "codex" });
    if cfg!(target_os = "macos") {
        for root in [
            user_home.join("Applications"),
            PathBuf::from("/Applications"),
        ] {
            for name in ["ChatGPT.app", "Codex.app"] {
                if let Some(path) = runtime_file(&root.join(name).join("Contents/Resources/codex"))
                {
                    return Some(path);
                }
            }
        }
    }
    if let Some(path) = runtime_file(&plugin) {
        return Some(path);
    }
    if cfg!(windows) {
        let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("OpenAI/Codex/bin");
        let children = entries(&root)?;
        if children.len() > 64 {
            return None;
        }
        let canonical = root.canonicalize().ok()?;
        return newest(
            std::iter::once(root.join("codex.exe"))
                .chain(
                    children
                        .into_iter()
                        .filter(|path| path.is_dir())
                        .map(|path| path.join("codex.exe")),
                )
                .filter_map(|path| runtime_file(&path))
                .filter(|path| {
                    path.parent() == Some(canonical.as_path())
                        || path.parent().and_then(Path::parent) == Some(canonical.as_path())
                }),
        );
    }
    None
}
pub(super) fn discover(
    home: &Path,
    user_home: &Path,
    configured: Option<&Path>,
    fallback: Option<&Path>,
) -> Vec<Candidate> {
    let mut result = Vec::new();
    let mut add = |source, name, path: PathBuf| {
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
            result.push(Candidate { source, name, path });
        }
    };
    if let Some(path) = configured {
        add("configured", "Configured Codex", path.to_owned());
    }
    if let Some(path) = app(home, user_home) {
        add("codex_app", "ChatGPT App (Codex)", path);
    }
    let root = home.join("packages/standalone/current");
    let names = if cfg!(windows) {
        ["codex.exe", "codex"]
    } else {
        ["codex", "codex.exe"]
    };
    if let Some(path) = [root.join("bin"), root]
        .into_iter()
        .flat_map(|root| names.map(|name| root.join(name)))
        .find(|path| executable(path))
    {
        add("managed", "Managed Codex runtime", path);
    }
    for (source, name, folder) in [
        ("vscode", "VS Code extension", ".vscode"),
        (
            "vscode_insiders",
            "VS Code Insiders extension",
            ".vscode-insiders",
        ),
        ("cursor", "Cursor extension", ".cursor"),
    ] {
        if let Some(path) = editor(&user_home.join(folder).join("extensions")) {
            add(source, name, path);
        }
    }
    if let Some(path) = path_cli().or_else(|| fallback.map(Path::to_owned)) {
        add("path_cli", "PATH Codex CLI", path);
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::path_cli_in;
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
        let path_var =
            std::env::join_paths([PathBuf::new(), relative, PathBuf::from(".")]).unwrap();
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
}
