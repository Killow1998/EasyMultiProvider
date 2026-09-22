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
    #[cfg(windows)]
    let names = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(|ext| format!("codex{ext}"))
        .collect::<Vec<_>>();
    #[cfg(not(windows))]
    let names = ["codex".to_owned()];
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|root| {
        names
            .iter()
            .map(|name| root.join(name))
            .find(|path| executable(path))
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
