//! Native agy discovery. Never invoke an installer or a shell during discovery.
use std::path::PathBuf;

pub fn resolve_agy_cli() -> Option<PathBuf> {
    let name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let mut directories: Vec<_> = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    if let Some(home) = home.map(PathBuf::from).filter(|p| p.is_absolute()) {
        directories.push(home.join(".local/bin"));
    }
    directories.into_iter().find_map(|directory| {
        let candidate = directory.join(name).canonicalize().ok()?;
        crate::executable_trust::validate_executable(&candidate).ok()?;
        Some(candidate)
    })
}
