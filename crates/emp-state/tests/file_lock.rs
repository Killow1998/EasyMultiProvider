use emp_state::{IntegrationFileLock, LockError};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn canonical_temp_root(directory: &tempfile::TempDir) -> std::path::PathBuf {
    directory.path().canonicalize().expect("canonical tempdir")
}

#[test]
fn lock_is_exclusive_times_out_and_is_released_on_drop() {
    let directory = tempdir().expect("tempdir");
    let path = canonical_temp_root(&directory).join("state/integration.lock");
    let first = IntegrationFileLock::acquire(&path, Duration::ZERO, Duration::from_millis(1))
        .expect("first lock");
    assert_eq!(
        IntegrationFileLock::acquire(&path, Duration::from_millis(20), Duration::from_millis(2),)
            .err()
            .expect("contention"),
        LockError::TimedOut(Duration::from_millis(20))
    );
    drop(first);
    IntegrationFileLock::acquire(&path, Duration::ZERO, Duration::ZERO).expect("released lock");

    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::metadata(&path)
                .expect("lock metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().expect("lock parent"))
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[cfg(unix)]
#[test]
fn lock_rejects_symlinked_parent_without_touching_target() {
    let directory = tempdir().expect("tempdir");
    let root = canonical_temp_root(&directory);
    let target = root.join("target");
    std::fs::create_dir(&target).expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("target mode");
    let linked = root.join("linked");
    symlink(&target, &linked).expect("parent symlink");
    assert_eq!(
        IntegrationFileLock::acquire(&linked.join("service.lock"), Duration::ZERO, Duration::ZERO,)
            .err()
            .expect("unsafe lock path"),
        LockError::PathUnsafe
    );
    assert!(!target.join("service.lock").exists());
    assert_eq!(
        std::fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn rust_lock_contends_with_python_when_oracle_is_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let directory = tempdir().expect("tempdir");
    let path = canonical_temp_root(&directory).join("shared/integration.lock");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let script = r#"
import pathlib, sys
from easy_multi_provider.integration import _FileLock
with _FileLock(pathlib.Path(sys.argv[1]), timeout=1.0):
    print("ready", flush=True)
    sys.stdin.read(1)
"#;
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(&path)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python lock holder");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("Python stdout"))
        .read_line(&mut line)
        .expect("Python readiness");
    assert_eq!(line.trim(), "ready");
    let contender =
        IntegrationFileLock::acquire(&path, Duration::from_millis(30), Duration::from_millis(2));
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(b"x")
        .expect("release Python lock");
    let output = child
        .wait_with_output()
        .expect("wait for Python lock holder");
    assert!(
        output.status.success(),
        "Python lock holder failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        contender.err().expect("cross-language contention"),
        LockError::TimedOut(Duration::from_millis(30))
    );

    let rust_holder = IntegrationFileLock::acquire(&path, Duration::ZERO, Duration::ZERO)
        .expect("lock after Python release");
    let contender = Command::new(std::env::var("EMP_PYTHON_INTEROP").expect("Python oracle"))
        .arg("-c")
        .arg(
            r#"
import pathlib, sys
from easy_multi_provider.integration import _FileLock, LockTimeout
try:
    with _FileLock(pathlib.Path(sys.argv[1]), timeout=0.03, poll_interval=0.002):
        raise AssertionError("Python acquired Rust-held lock")
except LockTimeout as exc:
    print(str(exc))
"#,
        )
        .arg(&path)
        .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("run Python contender");
    drop(rust_holder);
    assert!(
        contender.status.success(),
        "Python contender failed: {}",
        String::from_utf8_lossy(&contender.stderr)
    );
    assert!(
        String::from_utf8_lossy(&contender.stdout).contains("integration lock timed out"),
        "Python unexpectedly acquired the Rust lock"
    );
}
