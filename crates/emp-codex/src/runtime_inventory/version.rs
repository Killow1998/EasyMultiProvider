//! Bounded version observations; raw process output never reaches the UI.
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Lowest Codex version allowed by EMP's HTTP request gate for this release.
/// Passing the gate does not establish protocol or history compatibility.
pub const MINIMUM_CODEX: (u32, u32, u32) = (0, 158, 0);

pub fn minimum_codex() -> String {
    let (major, minor, patch) = MINIMUM_CODEX;
    format!("{major}.{minor}.{patch}")
}

pub(super) fn public(installed: Option<String>, status: &str) -> Value {
    json!({"installed":installed,"status":status,"minimum":minimum_codex()})
}

/// Returns the client's version when a request comes from a Codex build older
/// than [`MINIMUM_CODEX`]. Codex identifies itself as `<originator>/<version> (…)`;
/// other clients are never refused.
pub fn outdated_codex_client(user_agent: &str) -> Option<String> {
    let (originator, rest) = user_agent.split_once('/')?;
    if !originator.trim().to_ascii_lowercase().starts_with("codex") {
        return None;
    }
    let version = rest.split(|c: char| c.is_whitespace() || c == '(').next()?;
    let mut parts = version.split(['.', '-', '+']);
    let mut next = || parts.next()?.parse::<u32>().ok();
    let found = (next()?, next()?, next()?);
    (found < MINIMUM_CODEX).then(|| version.to_owned())
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.')
}
fn number(bytes: &[u8], position: &mut usize, limit: usize) -> Option<u32> {
    let start = *position;
    while bytes.get(*position).is_some_and(u8::is_ascii_digit) {
        *position += 1;
    }
    if *position == start || *position - start > limit {
        return None;
    }
    std::str::from_utf8(&bytes[start..*position])
        .ok()?
        .parse()
        .ok()
}
fn suffix(bytes: &[u8], position: &mut usize, prefix: u8) -> Option<String> {
    if bytes.get(*position) != Some(&prefix) {
        return Some(String::new());
    }
    let start = *position;
    *position += 1;
    while bytes
        .get(*position)
        .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
    {
        *position += 1;
    }
    (*position > start + 1).then(|| String::from_utf8_lossy(&bytes[start..*position]).into_owned())
}
fn parse(bytes: &[u8], start: usize) -> Option<(String, &'static str)> {
    let mut position = start + usize::from(bytes.get(start) == Some(&b'v'));
    let major = number(bytes, &mut position, 4)?;
    if bytes.get(position) != Some(&b'.') {
        return None;
    }
    position += 1;
    let minor = number(bytes, &mut position, 4)?;
    if bytes.get(position) != Some(&b'.') {
        return None;
    }
    position += 1;
    let patch = number(bytes, &mut position, 6)?;
    let prerelease = suffix(bytes, &mut position, b'-')?;
    let build = suffix(bytes, &mut position, b'+')?;
    if bytes
        .get(position)
        .is_some_and(|c| word(*c) || matches!(c, b'+' | b'-'))
    {
        return None;
    }
    let status = if (major, minor, patch) < MINIMUM_CODEX {
        "unsupported"
    } else {
        "supported"
    };
    Some((
        format!("{major}.{minor}.{patch}{prerelease}{build}"),
        status,
    ))
}
fn classify(output: &str) -> Value {
    let bytes = output.as_bytes();
    for start in 0..bytes.len() {
        if start > 0 && word(bytes[start - 1]) {
            continue;
        }
        if let Some((version, status)) = parse(bytes, start) {
            return public(Some(version), status);
        }
    }
    public(None, "unknown")
}

pub(super) fn observe(path: &Path) -> Value {
    observe_with_path(path, std::env::var_os("PATH"))
}

fn observe_with_path(path: &Path, inherited_path: Option<OsString>) -> Value {
    observe_inner(path, inherited_path).unwrap_or_else(|error| {
        public(
            None,
            if error.kind() == std::io::ErrorKind::NotFound {
                "unavailable"
            } else {
                "unknown"
            },
        )
    })
}
fn observe_inner(path: &Path, inherited_path: Option<OsString>) -> std::io::Result<Value> {
    // Never execute a candidate that fails the executable trust policy
    // (relative path, foreign owner, or writable ancestor directory).
    let launch_directory = super::launcher::launcher_directory(path);
    let Some(path) = super::trust::trusted_binary(path) else {
        return Ok(public(None, "unknown"));
    };
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut command = Command::new(&path);
    if let Some(directory) = launch_directory {
        match super::launcher::path_with_node(&directory, inherited_path.clone()) {
            super::launcher::PathAdjustment::Set(child_path) => {
                command.env("PATH", child_path);
            }
            super::launcher::PathAdjustment::Inherit => {
                if let Some(inherited_path) = inherited_path {
                    command.env("PATH", inherited_path);
                }
            }
            super::launcher::PathAdjustment::Refuse => {
                return Ok(public(None, "unknown"));
            }
        }
    } else if let Some(inherited_path) = inherited_path {
        command.env("PATH", inherited_path);
    }
    let mut child = command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline
            || output.metadata()?.len() > 2 * 1024 * 1024
            || errors.metadata()?.len() > 32 * 1024
        {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let Some(status) = status else {
        return Ok(public(None, "unknown"));
    };
    output.seek(SeekFrom::Start(0))?;
    errors.seek(SeekFrom::Start(0))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    output.take(2 * 1024 * 1024).read_to_end(&mut stdout)?;
    errors.take(32 * 1024).read_to_end(&mut stderr)?;
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    Ok(if status.success() {
        classify(&stdout)
    } else if status.code() == Some(127)
        || format!("{stdout} {stderr}")
            .to_lowercase()
            .contains("not found")
    {
        public(None, "unavailable")
    } else {
        public(None, "unknown")
    })
}

#[cfg(test)]
mod tests {
    use super::classify;
    use serde_json::json;

    #[test]
    fn versions_at_or_above_the_minimum_pass_the_version_gate() {
        for (output, installed, status) in [
            ("codex-cli 0.157.9", "0.157.9", "unsupported"),
            ("codex-cli 0.158.0", "0.158.0", "supported"),
            ("codex-cli 0.158.1", "0.158.1", "supported"),
            ("codex-cli 0.190.0-alpha.1", "0.190.0-alpha.1", "supported"),
            ("codex-cli 1.0.0", "1.0.0", "supported"),
        ] {
            assert_eq!(
                classify(output),
                json!({"installed":installed, "status":status, "minimum":"0.158.0"}),
                "classification for {output}"
            );
        }
    }

    #[test]
    fn only_outdated_codex_clients_are_refused() {
        use super::outdated_codex_client as outdated;
        assert_eq!(
            outdated("codex_cli_rs/0.157.9 (Ubuntu 24.4.0; x86_64) xterm"),
            Some("0.157.9".to_owned())
        );
        assert_eq!(outdated("codex_vscode/0.157.9"), Some("0.157.9".to_owned()));
        assert_eq!(
            outdated("Codex Desktop/0.157.9 (Mac OS 15; arm64)"),
            Some("0.157.9".to_owned())
        );
        assert_eq!(outdated("codex_cli_rs/0.158.0 (Ubuntu; x86_64)"), None);
        assert_eq!(outdated("codex-tui/0.200.0-alpha.3 (Linux)"), None);
        assert_eq!(outdated("omp/18.3.1"), None);
        assert_eq!(outdated("python-requests/2.31"), None);
        assert_eq!(outdated("codex_cli_rs/garbage"), None);
    }

    #[cfg(unix)]
    #[test]
    fn observe_never_runs_untrusted_candidates() {
        use crate::runtime_inventory::trust::tests::{private_dir, script};
        use std::os::unix::fs::PermissionsExt;
        let dir = private_dir();
        let marker = dir.path().join("ran");
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        let body = format!("touch '{}'\necho codex-cli 0.156.1", marker.display());
        let candidate = script(&shared, "codex", &body, 0o755);
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(super::observe(&candidate)["status"], "unknown");
        let relative = std::path::Path::new("codex");
        assert_eq!(super::observe(relative)["status"], "unknown");
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!marker.exists(), "untrusted candidate was executed");
    }

    #[cfg(unix)]
    #[test]
    fn observe_runs_trusted_candidates() {
        use crate::runtime_inventory::trust::tests::{ancestors_are_private, private_dir, script};
        let dir = private_dir();
        if !ancestors_are_private(dir.path()) {
            return;
        }
        let candidate = script(dir.path(), "codex", "echo codex-cli 0.158.0", 0o700);
        assert_eq!(super::observe(&candidate)["status"], "supported");
    }

    #[cfg(unix)]
    #[test]
    fn observe_runs_npm_cli_with_its_sibling_node_when_path_is_empty() {
        use crate::runtime_inventory::trust::tests::{ancestors_are_private, private_dir, script};
        use std::ffi::OsString;
        use std::os::unix::fs::symlink;

        let root = private_dir();
        if !ancestors_are_private(root.path()) {
            return;
        }
        let node_version = format!("v{}", env!("CARGO_PKG_VERSION"));
        let bin = root
            .path()
            .join("versions/node")
            .join(&node_version)
            .join("bin");
        let package = root
            .path()
            .join("versions/node")
            .join(node_version)
            .join("lib/node_modules/@openai/codex/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        script(
            &bin,
            "node",
            "script=$1; shift; exec /bin/sh \"$script\" \"$@\"",
            0o700,
        );
        let cli = script(&package, "codex.js", "exit 1", 0o700);
        std::fs::write(&cli, "#!/usr/bin/env node\necho codex-cli 0.158.0\n").unwrap();
        let launcher = bin.join("codex");
        symlink(&cli, &launcher).unwrap();

        let path = std::env::join_paths([&bin]).unwrap();
        let discovered = super::super::discovery::path_cli_in(&path)
            .expect("PATH discovery should retain the npm launcher");
        assert_eq!(discovered, launcher);

        let observed = super::observe_with_path(&discovered, Some(OsString::new()));
        assert_eq!(observed["installed"], "0.158.0");
        assert_eq!(observed["status"], "supported");
    }
}
