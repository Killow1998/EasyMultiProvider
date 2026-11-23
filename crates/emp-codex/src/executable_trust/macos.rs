#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ENGINE_REQUIREMENT: &str = "=anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"2DC432GLL2\" and identifier \"codex\"";
const ENGINE_RELATIVE: &str = "Contents/MacOS/codex";

pub(super) fn known_bundle(path: &Path) -> Option<PathBuf> {
    ["ChatGPT.app", "Codex.app"].iter().find_map(|app| {
        let bundle = Path::new("/Applications")
            .join(app)
            .join("Contents/Resources/codex-cli/CodexCLI.app");
        (path == bundle.join(ENGINE_RELATIVE)).then_some(bundle)
    })
}

#[cfg(target_os = "macos")]
pub(super) fn is_shared_applications(path: &Path, info: &std::fs::Metadata) -> bool {
    path == Path::new("/Applications")
        && info.is_dir()
        && info.uid() == 0
        && info.mode() & 0o022 == 0o020
        && info.mode() & 0o6000 == 0
}

pub(super) fn verify_bundle(bundle: &Path, executable: &Path) -> bool {
    let mut command = Command::new("/usr/bin/codesign");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args([
            "--verify",
            "--strict",
            "--all-architectures",
            "--deep",
            "-R",
            ENGINE_REQUIREMENT,
        ])
        .arg(bundle);
    run_verifier(&mut command, Duration::from_secs(10)) && bundle_binds_engine(bundle, executable)
}

fn bundle_binds_engine(bundle: &Path, executable: &Path) -> bool {
    if bundle.join(ENGINE_RELATIVE).canonicalize().ok().as_deref() != Some(executable) {
        return false;
    }
    let Ok(info) = emp_state::read_file_limited(&bundle.join("Contents/Info.plist"), 256 * 1024)
    else {
        return false;
    };
    let Ok(info) = plist::Value::from_reader(std::io::Cursor::new(info)) else {
        return false;
    };
    info.as_dictionary()
        .and_then(|dictionary| dictionary.get("CFBundleExecutable"))
        .and_then(plist::Value::as_string)
        == Some("codex")
}

fn run_verifier(command: &mut Command, timeout: Duration) -> bool {
    let started = Instant::now();
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success() && started.elapsed() <= timeout,
            Ok(None) if started.elapsed() < timeout => {
                #[expect(
                    clippy::disallowed_methods,
                    reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
                )]
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests;
