use std::{path::PathBuf, sync::OnceLock, time::Duration};

pub fn fixture_root() -> PathBuf {
    std::env::var_os("EMP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn fixture_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(fixture_root())
}

pub fn test_timeout(duration: Duration) -> Duration {
    static SCALE: OnceLock<f64> = OnceLock::new();
    let scale = SCALE.get_or_init(|| match std::env::var("EMP_TEST_TIME_SCALE") {
        Ok(value) => value
            .parse::<f64>()
            .ok()
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .expect("EMP_TEST_TIME_SCALE must be a positive finite number"),
        Err(std::env::VarError::NotPresent) => 1.0,
        Err(error) => panic!("EMP_TEST_TIME_SCALE: {error}"),
    });
    duration.mul_f64(*scale)
}

#[cfg(unix)]
pub fn write_executable(path: &std::path::Path, script: &str) {
    let status = std::process::Command::new("/bin/sh")
        .args([
            "-ec",
            "umask 077; printf '%s' \"$2\" > \"$1\"; chmod 700 \"$1\"",
            "fixture",
        ])
        .arg(path)
        .arg(script)
        .status()
        .expect("write fixture executable");
    assert!(
        status.success(),
        "write fixture executable at {path:?}: {status}"
    );
}
