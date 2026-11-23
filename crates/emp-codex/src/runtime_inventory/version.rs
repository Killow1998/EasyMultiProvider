use crate::executable_trust::TrustFailure;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn public(installed: Option<String>, status: &str) -> Value {
    json!({"installed":installed,"status":status})
}

pub(super) fn failed(reason: &str) -> Value {
    json!({"installed":null,"status":if reason == "not_found" { "unavailable" } else { "unknown" },"failure":reason})
}

pub(super) fn trust_failure(error: TrustFailure) -> &'static str {
    match error {
        TrustFailure::Unavailable => "not_found",
        TrustFailure::NotTrusted => "untrusted_path",
        TrustFailure::Writable => "writable_path",
        #[cfg(target_os = "macos")]
        TrustFailure::BundleVerificationFailed => "bundle_verification_failed",
    }
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
fn parse(bytes: &[u8], start: usize) -> Option<String> {
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
    Some(format!("{major}.{minor}.{patch}{prerelease}{build}"))
}
fn classify(output: &str) -> Value {
    let bytes = output.as_bytes();
    for start in 0..bytes.len() {
        if start > 0 && word(bytes[start - 1]) {
            continue;
        }
        if let Some(version) = parse(bytes, start) {
            return public(Some(version), "available");
        }
    }
    failed("unrecognized_version")
}

pub(super) fn observe(path: &Path) -> Value {
    observe_with_path(path, std::env::var_os("PATH"))
}

fn observe_with_path(path: &Path, inherited_path: Option<OsString>) -> Value {
    let prepared = match crate::executable_trust::PreparedExecutable::prepare(path) {
        Ok(prepared) => prepared,
        Err(error) => return failed(trust_failure(error)),
    };
    let mut observation =
        probe(&prepared, path, inherited_path).unwrap_or_else(|_| failed("probe_io"));
    observation["launch_source"] = json!("original");
    observation
}

fn probe(
    prepared: &crate::PreparedExecutable,
    selector: &Path,
    inherited_path: Option<OsString>,
) -> std::io::Result<Value> {
    let launch_directory = super::launcher::launcher_directory(selector);
    let mut output = tempfile::tempfile()?;
    let errors = tempfile::tempfile()?;
    let mut command = Command::new(prepared.path());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
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
                return Ok(failed("untrusted_launcher"));
            }
        }
    } else if let Some(inherited_path) = inherited_path {
        command.env("PATH", inherited_path);
    }
    let mut child = match command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let mut result = failed("spawn_failed");
            result["os_error"] = json!(error.raw_os_error());
            return Ok(result);
        }
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let observed = (|| -> std::io::Result<_> {
        Ok(loop {
            if let Some(status) = child.try_wait()? {
                break Ok(status);
            }
            let failure = if Instant::now() >= deadline {
                Some("probe_timeout")
            } else if output.metadata()?.len() > 2 * 1024 * 1024
                || errors.metadata()?.len() > 32 * 1024
            {
                Some("probe_output_limit")
            } else {
                None
            };
            if let Some(failure) = failure {
                let _ = child.kill();
                let _ = child.wait();
                break Err(failure);
            }
            std::thread::sleep(Duration::from_millis(10));
        })
    })();
    let status = match observed {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(failure) => return Ok(failed(failure)),
    };
    if !status.success() {
        let mut result = failed("process_failed");
        result["exit_code"] = json!(status.code());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            result["signal"] = json!(status.signal());
        }
        return Ok(result);
    }
    output.seek(SeekFrom::Start(0))?;
    let mut stdout = Vec::new();
    output.take(2 * 1024 * 1024).read_to_end(&mut stdout)?;
    Ok(classify(&String::from_utf8_lossy(&stdout)))
}

#[cfg(test)]
mod tests {
    use super::classify;
    use serde_json::json;

    #[test]
    fn version_observation_does_not_assert_protocol_compatibility() {
        for (output, installed, status) in [
            (
                "codex-cli 0.155.0-alpha.9.2",
                "0.155.0-alpha.9.2",
                "available",
            ),
            ("codex-cli 0.157.9", "0.157.9", "available"),
            ("codex-cli 0.158.0", "0.158.0", "available"),
            ("codex-cli 0.158.1", "0.158.1", "available"),
            ("codex-cli 0.190.0-alpha.1", "0.190.0-alpha.1", "available"),
            ("codex-cli 1.0.0", "1.0.0", "available"),
        ] {
            assert_eq!(
                classify(output),
                json!({"installed":installed, "status":status}),
                "classification for {output}"
            );
        }
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
        assert_eq!(super::observe(&candidate)["status"], "available");
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
        assert_eq!(observed["status"], "available");
    }
}
