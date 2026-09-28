use emp_state::{IntegrationFileLock, LockError};
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
