#[cfg(any(unix, windows))]
use super::probe_candidate_version;
use super::{UpdateManager, nonce_with};
use crate::update::release::UpdateEndpoints;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
fn candidate_script(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let binary = root.join(name);
    std::fs::write(&binary, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    binary
}

#[cfg(unix)]
#[test]
fn candidate_version_probe_accepts_only_successful_exact_output() {
    let root = tempfile::TempDir::new().unwrap();
    let valid = candidate_script(root.path(), "valid", "printf 'EMP 0.12.0\\n'");
    assert!(probe_candidate_version(&valid, "0.12.0", root.path(), Duration::from_secs(1)).is_ok());

    let nonzero = candidate_script(root.path(), "nonzero", "printf 'EMP 0.12.0\\n'; exit 7");
    assert_eq!(
        probe_candidate_version(&nonzero, "0.12.0", root.path(), Duration::from_secs(1))
            .unwrap_err(),
        crate::update::UpdateError("version_mismatch")
    );
    assert_eq!(
        probe_candidate_version(&valid, "0.12.1", root.path(), Duration::from_secs(1)).unwrap_err(),
        crate::update::UpdateError("version_mismatch")
    );
}

#[cfg(unix)]
#[test]
fn candidate_version_probe_maps_spawn_failures_like_python() {
    let root = tempfile::TempDir::new().unwrap();
    assert_eq!(
        probe_candidate_version(
            &root.path().join("missing"),
            "0.12.0",
            root.path(),
            Duration::from_secs(1)
        )
        .unwrap_err(),
        crate::update::UpdateError("update_failed")
    );
    let denied = candidate_script(root.path(), "denied", "exit 0");
    std::fs::set_permissions(&denied, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    assert_eq!(
        probe_candidate_version(&denied, "0.12.0", root.path(), Duration::from_secs(1))
            .unwrap_err(),
        crate::update::UpdateError("directory_not_writable")
    );
}

#[cfg(unix)]
#[test]
fn candidate_version_probe_times_out_and_reaps_the_child() {
    let root = tempfile::TempDir::new().unwrap();
    let hanging = candidate_script(
        root.path(),
        "hanging",
        "printf '%s\\n' \"$$\"; exec sleep 5",
    );
    let started = Instant::now();
    assert_eq!(
        probe_candidate_version(&hanging, "0.12.0", root.path(), Duration::from_millis(100))
            .unwrap_err(),
        crate::update::UpdateError("update_failed")
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid: i32 = std::fs::read_to_string(root.path().join("candidate-version.stdout"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[cfg(windows)]
#[test]
fn candidate_version_probe_restores_error_mode_after_bad_image() {
    use windows_sys::Win32::System::Diagnostics::Debug::GetThreadErrorMode;

    let root = tempfile::TempDir::new().unwrap();
    let binary = root.path().join("candidate.exe");
    std::fs::write(&binary, b"deliberately invalid executable").unwrap();
    let previous = unsafe { GetThreadErrorMode() };
    assert_eq!(
        probe_candidate_version(&binary, "0.12.0", root.path(), Duration::from_secs(1))
            .unwrap_err(),
        crate::update::UpdateError("update_failed")
    );
    assert_eq!(unsafe { GetThreadErrorMode() }, previous);
}

#[test]
fn concurrent_checks_start_only_one_release_job() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let release_gate = Arc::new(Barrier::new(3));
    let server_gate = Arc::clone(&release_gate);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "release request closed before its headers");
            request.extend_from_slice(&buffer[..count]);
        }
        server_gate.wait();
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
        drop(stream);
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut requests = 1;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut extra, _)) => {
                    requests += 1;
                    let mut sink = [0_u8; 512];
                    let _ = extra.read(&mut sink);
                    let _ = extra.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fake release server failed: {error}"),
            }
        }
        requests
    });
    let manager = UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        Vec::new(),
        env!("CARGO_PKG_VERSION"),
        UpdateEndpoints::for_source(&base, format!("{base}/latest")),
        || Err(crate::update::UpdateError("worker_failed")),
    )
    .unwrap();
    let callers_gate = Arc::new(Barrier::new(3));
    let first_manager = manager.clone();
    let first_callers_gate = Arc::clone(&callers_gate);
    let first_release_gate = Arc::clone(&release_gate);
    let first = thread::spawn(move || {
        first_callers_gate.wait();
        let snapshot = first_manager.start("check").unwrap();
        assert_eq!(snapshot.state, "checking");
        first_release_gate.wait();
    });
    let second_manager = manager.clone();
    let second_callers_gate = Arc::clone(&callers_gate);
    let second_release_gate = Arc::clone(&release_gate);
    let second = thread::spawn(move || {
        second_callers_gate.wait();
        let snapshot = second_manager.start("check").unwrap();
        assert_eq!(snapshot.state, "checking");
        second_release_gate.wait();
    });
    callers_gate.wait();
    first.join().unwrap();
    second.join().unwrap();
    assert_eq!(server.join().unwrap(), 1, "only one check request is sent");
    let deadline = Instant::now() + Duration::from_secs(3);
    while manager.snapshot().state == "checking" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(manager.snapshot().state, "no_release");
}

#[test]
fn startup_rollback_is_visible_in_the_initial_snapshot() {
    let manager = UpdateManager::with_startup_result(
        std::env::current_exe().unwrap(),
        Vec::new(),
        env!("CARGO_PKG_VERSION"),
        UpdateEndpoints::default(),
        true,
        || Err(crate::update::UpdateError("worker_failed")),
    )
    .unwrap();
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.state, "error");
    assert_eq!(snapshot.error, "install_rolled_back");
}

#[test]
fn nonce_random_source_failure_is_returned() {
    let error = nonce_with(|_| Err(getrandom::Error::UNSUPPORTED)).unwrap_err();
    assert_eq!(error, crate::update::UpdateError("update_failed"));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn dpkg_owned_install_requires_manual_migration_before_any_download() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    let directory = tempfile::tempdir().unwrap();
    let query = directory.path().join("dpkg-query");
    fs::write(
        &query,
        "#!/bin/sh\nif [ \"$1\" = --search ]; then printf 'easy-multi-provider: /usr/bin/EMP\\n'; else printf 'install ok installed\\t0.11.2'; fi\n",
    )
    .unwrap();
    fs::set_permissions(&query, fs::Permissions::from_mode(0o700)).unwrap();

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let release = serde_json::json!({
        "tag_name":"v9.0.0", "draft":false, "prerelease":false,
        "assets":[{"name":"EMP-linux-x86_64.tar.gz", "digest":format!("sha256:{}", "a".repeat(64)),
            "size":7, "browser_download_url":format!("{base}/releases/download/v9.0.0/EMP-linux-x86_64.tar.gz") }]
    })
    .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{release}",
            release.len()
        )
        .unwrap();
    });
    let mut manager = UpdateManager::with_endpoints(
        PathBuf::from("/usr/bin/EMP"),
        Vec::new(),
        "0.11.2",
        UpdateEndpoints::for_source(&base, format!("{base}/latest")),
        || Err(crate::update::UpdateError("worker_failed")),
    )
    .unwrap();
    Arc::get_mut(&mut manager.0).unwrap().dpkg_query = query;
    manager.check().unwrap();
    server.join().unwrap();
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.state, "migration_required");
    assert_eq!(snapshot.manual_update, "linux_system_migration");
    assert_eq!(
        manager.start("install").unwrap_err(),
        crate::update::UpdateError("update_unavailable")
    );

    // Ownership can change after a check: the installer must reject it before opening
    // the package URL or creating a replacement job.
    manager.0.snapshot.lock().unwrap().manual_update.clear();
    assert_eq!(
        manager.install(),
        Err(crate::update::UpdateError("system_install_manual"))
    );
}
