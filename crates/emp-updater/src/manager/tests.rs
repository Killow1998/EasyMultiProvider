#[cfg(any(unix, windows))]
use super::probe_candidate_version;
use super::{UpdateManager, nonce_with};
use crate::release::UpdateEndpoints;
use emp_test_support::{Deadline, accept_blocking, test_timeout};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

mod catalog;
mod retry;

#[cfg(unix)]
fn candidate_script(root: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let binary = root.join(name);
    emp_test_support::write_executable(&binary, &format!("#!/bin/sh\n{body}\n"));
    binary
}

#[cfg(unix)]
#[test]
fn candidate_version_probe_accepts_only_successful_exact_output() {
    let root = emp_test_support::fixture_dir().unwrap();
    let valid = candidate_script(root.path(), "valid", "printf 'EMP 0.12.0\\n'");
    probe_candidate_version(
        &valid,
        "0.12.0",
        root.path(),
        test_timeout(Duration::from_secs(1)),
        None,
        |_| {},
    )
    .expect("valid candidate version probe should succeed");

    let nonzero = candidate_script(root.path(), "nonzero", "printf 'EMP 0.12.0\\n'; exit 7");
    assert_eq!(
        probe_candidate_version(
            &nonzero,
            "0.12.0",
            root.path(),
            test_timeout(Duration::from_secs(1)),
            None,
            |_| {},
        )
        .unwrap_err(),
        crate::UpdateError("version_mismatch")
    );
    assert_eq!(
        probe_candidate_version(
            &valid,
            "0.12.1",
            root.path(),
            test_timeout(Duration::from_secs(1)),
            None,
            |_| {},
        )
        .unwrap_err(),
        crate::UpdateError("version_mismatch")
    );
}

#[cfg(unix)]
#[test]
fn candidate_version_probe_maps_spawn_failures_to_update_errors() {
    let root = emp_test_support::fixture_dir().unwrap();
    assert_eq!(
        probe_candidate_version(
            &root.path().join("missing"),
            "0.12.0",
            root.path(),
            test_timeout(Duration::from_secs(1)),
            None,
            |_| {},
        )
        .unwrap_err(),
        crate::UpdateError("update_failed")
    );
    let denied = candidate_script(root.path(), "denied", "exit 0");
    std::fs::set_permissions(&denied, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
    assert_eq!(
        probe_candidate_version(
            &denied,
            "0.12.0",
            root.path(),
            test_timeout(Duration::from_secs(1)),
            None,
            |_| {},
        )
        .unwrap_err(),
        crate::UpdateError("directory_not_writable")
    );
}

#[cfg(unix)]
#[test]
fn candidate_version_probe_times_out_and_reaps_the_child() {
    let root = emp_test_support::fixture_dir().unwrap();
    let binary = candidate_script(root.path(), "slow", "exec sleep 60");
    let manager = UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        Vec::new(),
        "0.12.0",
        UpdateEndpoints::default(),
        || Ok(()),
    )
    .unwrap();
    manager.stage("verify_version");
    let mut spawned_pid = None;
    let started = std::time::Instant::now();
    let error = probe_candidate_version(
        &binary,
        "0.12.0",
        root.path(),
        test_timeout(Duration::from_millis(100)),
        Some(&manager),
        |child| spawned_pid = Some(child.id()),
    )
    .unwrap_err();
    assert!(started.elapsed() < test_timeout(Duration::from_secs(2)));
    assert_eq!(error, crate::UpdateError("update_failed"));
    manager.failed(error);
    let receipt = manager.snapshot().failure.unwrap();
    assert_eq!(receipt.reason.as_deref(), Some("timeout"));

    let pid = i32::try_from(spawned_pid.expect("probe started a child")).unwrap();
    // SAFETY: signal zero only probes the fixture PID and kill takes no pointers.
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

    let root = emp_test_support::fixture_dir().unwrap();
    let binary = root.path().join("candidate.exe");
    std::fs::write(&binary, b"deliberately invalid executable").unwrap();
    // SAFETY: GetThreadErrorMode takes no arguments and reads only this thread.
    let previous = unsafe { GetThreadErrorMode() };
    assert_eq!(
        probe_candidate_version(
            &binary,
            "0.12.0",
            root.path(),
            test_timeout(Duration::from_secs(1)),
            None,
            |_| {},
        )
        .unwrap_err(),
        crate::UpdateError("update_failed")
    );
    // SAFETY: GetThreadErrorMode takes no arguments and reads only this thread.
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
        let deadline = Deadline::after(test_timeout(Duration::from_millis(300)));
        let mut requests = 1;
        while !deadline.is_expired() {
            match accept_blocking(&listener) {
                Ok((extra, _)) => {
                    requests += 1;
                    let mut extra = deadline.io(extra);
                    let mut sink = [0_u8; 512];
                    let _ = extra.read(&mut sink);
                    let _ = extra.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                    )]
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
        || Err(crate::UpdateError("worker_failed")),
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
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    while manager.snapshot().state == "checking" && !deadline.is_expired() {
        #[expect(
            clippy::disallowed_methods,
            reason = "并发发布检查通过状态快照观察，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(5));
    }
    let snapshot = deadline
        .accept(manager.snapshot(), "concurrent release check finished")
        .unwrap();
    assert_eq!(snapshot.state, "no_release");
}

#[test]
fn startup_rollback_is_visible_in_the_initial_snapshot() {
    let manager = UpdateManager::with_startup_result(
        std::env::current_exe().unwrap(),
        Vec::new(),
        env!("CARGO_PKG_VERSION"),
        UpdateEndpoints::default(),
        true,
        || Err(crate::UpdateError("worker_failed")),
    )
    .unwrap();
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.state, "error");
    assert_eq!(snapshot.error, "install_rolled_back");
}

#[test]
fn nonce_random_source_failure_is_returned() {
    let error = nonce_with(|_| Err(getrandom::Error::UNSUPPORTED)).unwrap_err();
    assert_eq!(error, crate::UpdateError("update_failed"));
}

#[cfg(target_os = "linux")]
#[test]
fn failed_download_receipt_survives_staging_cleanup_and_restart_without_secrets() {
    let root = emp_test_support::fixture_dir().unwrap();
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
    assert_eq!(server.join().unwrap(), 4);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(12)));
    while manager.snapshot().state != "error" && !deadline.is_expired() {
        #[expect(
            clippy::disallowed_methods,
            reason = "更新阶段通过状态快照观察，以固定间隔轮询"
        )]
        thread::sleep(Duration::from_millis(5));
    }
    let snapshot = deadline
        .accept(manager.snapshot(), "failed download snapshot")
        .unwrap();
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
    let root = emp_test_support::fixture_dir().unwrap();
    let manager = UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        Vec::new(),
        "0.12.6",
        UpdateEndpoints::default(),
        || Ok(()),
    )
    .unwrap();
    manager.stage("verify_version");
    let error = probe_candidate_version(
        &root.path().join("absent"),
        "0.12.6",
        root.path(),
        test_timeout(Duration::from_secs(1)),
        Some(&manager),
        |_| {},
    )
    .unwrap_err();
    manager.failed(error);
    let receipt = manager.snapshot().failure.unwrap();
    assert_eq!(receipt.stage, "verify_version");
    assert_eq!(receipt.os_error, Some(libc::ENOENT));
}

#[cfg(target_os = "linux")]
#[test]
fn dpkg_owned_install_requires_manual_migration_before_any_download() {
    use std::path::PathBuf;

    let directory = emp_test_support::fixture_dir().unwrap();
    let query = candidate_script(
        directory.path(),
        "dpkg-query",
        "if [ \"$1\" = --search ]; then printf 'easy-multi-provider: /usr/bin/EMP\\n'; else printf 'install ok installed\\t0.11.2'; fi",
    );

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let name = crate::release::current_asset_name().unwrap();
    let release = serde_json::json!({
        "tag_name":"v9.0.0", "draft":false, "prerelease":false,
        "assets":[{"name":name, "digest":format!("sha256:{}", "a".repeat(64)),
            "size":7, "browser_download_url":format!("{base}/releases/download/v9.0.0/{name}") }]
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
        || Err(crate::UpdateError("worker_failed")),
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
        crate::UpdateError("update_unavailable")
    );

    // Ownership can change after a check: the installer must reject it before opening
    // the package URL or creating a replacement job.
    manager.0.snapshot.lock().unwrap().manual_update.clear();
    assert_eq!(
        manager.install(),
        Err(crate::UpdateError("system_install_manual"))
    );
}

#[test]
fn update_journal_keeps_retry_stage_timings_after_success() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for status in ["503 Service Unavailable", "404 Not Found"] {
            let (stream, _) = listener.accept().unwrap();
            let mut stream =
                emp_test_support::Deadline::after(test_timeout(Duration::from_secs(3))).io(stream);
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
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    loop {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == "update_finished")
        {
            deadline.accept((), "update finished event").unwrap();
            break;
        }
        assert!(!deadline.is_expired(), "update never completed");
        #[expect(
            clippy::disallowed_methods,
            reason = "更新完成通过观测回调报告，以固定间隔轮询已收集事件"
        )]
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

#[test]
fn release_check_retains_notes_without_starting_installation() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let name = crate::release::current_asset_name().unwrap();
    let notes = "## Fixes\n- Restore and layout corrections\n<script>sample text</script>";
    let body = serde_json::to_vec(&serde_json::json!({
        "tag_name":"v9.0.0","draft":false,"prerelease":false,"body":notes,
        "assets":[{"name":name,"digest":format!("sha256:{}","a".repeat(64)),"size":8,
        "browser_download_url":format!("{base}/releases/download/v9.0.0/{name}")}]
    }))
    .unwrap();
    let response = [
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes(),
        body,
    ]
    .concat();
    let server = thread::spawn(move || retry::serve_responses(listener, vec![response]));
    let manager = UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        Vec::new(),
        env!("CARGO_PKG_VERSION"),
        UpdateEndpoints::for_source(&base, format!("{base}/latest")),
        || panic!("checking release notes must never install or exit"),
    )
    .unwrap();
    manager.check().unwrap();
    assert_eq!(server.join().unwrap(), 1);
    assert_eq!(manager.snapshot().state, "available");
    assert_eq!(manager.snapshot().release_notes, notes);
    assert_eq!(manager.snapshot().latest_version.as_deref(), Some("9.0.0"));
}
