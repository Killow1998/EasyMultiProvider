#[cfg(any(unix, windows))]
use super::probe_candidate_version;
use super::{UpdateManager, nonce_with};
use crate::update::release::UpdateEndpoints;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

mod retry;

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
    probe_candidate_version(&valid, "0.12.0", root.path(), Duration::from_secs(1))
        .expect("valid candidate version probe should succeed");

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
fn candidate_version_probe_maps_spawn_failures_to_update_errors() {
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

#[cfg(target_os = "linux")]
#[test]
fn failed_download_receipt_survives_staging_cleanup_and_restart_without_secrets() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("EMP");
    std::fs::write(&binary, b"original executable").unwrap();
    let args = vec![
        "--config".into(),
        root.path()
            .join("config.json")
            .to_string_lossy()
            .into_owned(),
    ];
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        retry::serve_responses(listener, vec![
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
            4
        ])
    });
    let endpoints = UpdateEndpoints::for_source(&base, format!("{base}/latest"));
    let manager = UpdateManager::with_endpoints(
        binary.clone(),
        args.clone(),
        "0.12.6",
        endpoints.clone(),
        || panic!("failed download must not exit EMP"),
    )
    .unwrap();
    *manager.0.asset.lock().unwrap() = Some(super::Asset {
        version: "9.0.0".into(),
        name: "EMP-linux-x86_64.tar.gz".into(),
        url: format!("{base}/package?token=secret-never-log"),
        size: 7,
        digest: "a".repeat(64),
    });
    manager.start("install").unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    while manager.snapshot().state != "error" && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.join().unwrap(), 4);
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.error, "update_failed");
    let receipt = snapshot.failure.unwrap();
    assert_eq!(receipt.stage, "download_package");
    assert_eq!(receipt.http_status, Some(503));
    assert_eq!(receipt.retry_count, 3);
    assert_eq!(receipt.retry_limit, 3);
    assert_eq!(std::fs::read(&binary).unwrap(), b"original executable");
    assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".emp-update-")
    }));
    let raw = std::fs::read_to_string(root.path().join("state/update-last-error.json")).unwrap();
    assert!(!raw.contains("secret-never-log"));
    assert!(!raw.contains(&base));
    let reopened =
        UpdateManager::with_endpoints(binary, args, "0.12.6", endpoints, || Ok(())).unwrap();
    let receipt = reopened.snapshot().failure.unwrap();
    assert_eq!(receipt.http_status, Some(503));
    assert_eq!(receipt.retry_count, 3);
    assert_eq!(receipt.retry_limit, 3);
}

#[cfg(unix)]
#[test]
fn failed_candidate_launch_preserves_the_original_system_error_code() {
    let root = tempfile::tempdir().unwrap();
    let manager = UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        Vec::new(),
        "0.12.6",
        UpdateEndpoints::default(),
        || Ok(()),
    )
    .unwrap();
    manager.stage("verify_version");
    let error = super::probe_candidate_version_observed(
        &root.path().join("absent"),
        "0.12.6",
        root.path(),
        Duration::from_secs(1),
        Some(&manager),
    )
    .unwrap_err();
    manager.failed(error);
    let receipt = manager.snapshot().failure.unwrap();
    assert_eq!(receipt.stage, "verify_version");
    assert_eq!(receipt.os_error, Some(libc::ENOENT));
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

#[test]
fn update_journal_keeps_retry_stage_timings_after_success() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for status in ["503 Service Unavailable", "404 Not Found"] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 512];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert_ne!(read, 0);
                request.extend_from_slice(&buffer[..read]);
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
    });
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let manager = UpdateManager::with_startup_hooks(
        std::env::current_exe().unwrap(),
        Vec::new(),
        env!("CARGO_PKG_VERSION"),
        UpdateEndpoints::for_source(&base, format!("{base}/latest")),
        false,
        super::UpdateHooks::new(|| Ok(()), || {}, || Ok(())).with_observer(move |event, fields| {
            observed
                .lock()
                .unwrap()
                .push((event.to_owned(), fields.clone()));
        }),
    )
    .unwrap();
    manager.start("check").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == "update_finished")
        {
            break;
        }
        assert!(Instant::now() < deadline, "update never completed");
        thread::sleep(Duration::from_millis(10));
    }
    server.join().unwrap();
    assert_eq!(manager.snapshot().state, "no_release");
    let events = events.lock().unwrap();
    let retry = events
        .iter()
        .find(|(name, _)| name == "update_retry")
        .unwrap();
    assert_eq!(retry.1["http_status"], 503);
    assert_eq!(retry.1["retry_count"], 1);
    let stages: Vec<_> = events
        .iter()
        .filter(|(name, _)| name == "update_stage_finished")
        .collect();
    assert_eq!(stages.len(), 2);
    assert_eq!(stages[0].1["outcome"], "retrying");
    assert_eq!(stages[1].1["outcome"], "completed");
    assert!(
        stages
            .iter()
            .all(|(_, fields)| fields["duration_ms"].is_number())
    );
    let finished = &events.last().unwrap().1;
    assert_eq!(finished["outcome"], "completed");
    assert_eq!(finished["retry_count"], 1);
    assert!(finished["duration_ms"].is_number());
}
