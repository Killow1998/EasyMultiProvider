use std::{path::Path, process::Command};

pub fn write_executable(path: &Path, script: &str) {
    let status = Command::new("/bin/sh")
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
