use super::*;
use emp_test_support::{Deadline, Observation, test_timeout};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

const ORIGINAL: &str = "# user settings\nopenai_base_url = \"https://native.example/v1\"\n[features]\nunified_exec = true\n";

fn assert_original(actual: &str) {
    let lines = |text: &str| {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(lines(actual), lines(ORIGINAL));
}

fn fixture() -> (tempfile::TempDir, ServerHandle) {
    let directory = emp_test_support::fixture_dir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let codex = root.join("codex");
    std::fs::create_dir(&codex).unwrap();
    std::fs::write(codex.join("config.toml"), ORIGINAL).unwrap();
    let server = ServerHandle::start_with_config_options(
        "127.0.0.1".parse().unwrap(),
        0,
        &root.join("emp.json"),
        "missing-test-codex",
        codex.join("auth.json"),
    )
    .unwrap();
    let integration = &server.state.backend.integration;
    assert!(
        integration
            .manager
            .enable_with_options(
                &server.state.base_url,
                Some("synthetic-models.json"),
                true,
                Some(&server.state.base_url),
                None,
            )
            .unwrap()
            .ok()
    );
    integration.search.apply(true).unwrap();
    integration.owned.store(true, Ordering::Release);
    (directory, server)
}

#[test]
fn exit_restores_before_active_requests_and_websockets_release() {
    let (_directory, server) = fixture();
    let state = Arc::clone(&server.state);
    let request = state.lifecycle.admission.acquire_request().unwrap();
    let websocket = state.lifecycle.admission.acquire_websocket().unwrap();
    let start = Instant::now();
    let shutdown = std::thread::spawn(move || server.shutdown());
    while state.backend.integration.manager.status().unwrap().state != "restored" {
        assert!(start.elapsed() < Duration::from_secs(2));
        #[expect(
            clippy::disallowed_methods,
            reason = "集成恢复状态通过快照观察，以固定间隔轮询"
        )]
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_original(
        &std::fs::read_to_string(state.backend.integration.manager.config_path()).unwrap(),
    );
    assert!(state.lifecycle.admission.acquire_request().is_none());
    assert!(state.lifecycle.admission.acquire_websocket().is_none());
    assert!(
        !shutdown.is_finished(),
        "exit waits for admitted handlers after restoring"
    );
    drop((request, websocket));
    shutdown.join().unwrap().unwrap();
}

fn hold_journal_event(
    state: &ServerState,
    event: &'static str,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, writing) = mpsc::channel();
    let (resume, release) = mpsc::channel();
    state
        .backend
        .diagnostics
        .journal
        .gate_next_event(event, move || {
            let _ = entered.send(());
            let _ = release.recv();
        });
    (writing, resume)
}

#[test]
fn final_journal_flush_waits_for_the_exit_restore_receipt() {
    let (directory, server) = fixture();
    let state = Arc::clone(&server.state);
    let callback_state = Arc::clone(&state);
    let root = directory.path().to_owned();
    let callback = Observation::spawn(move |probe| {
        let result = prepare_exit(&callback_state, false);
        probe.observe(|_| result.is_ok());
        probe.observe(|_| {
            confirm_session_exit_restore(&callback_state, result);
            crate::tests::live_journal(&root).iter().any(|record| {
                record["event"] == "session_exit_restore" && record["fields"]["success"] == true
            })
        });
    });
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    assert!(
        callback
            .begin(deadline, "exit callback restored configuration")
            .wait()
            .unwrap()
    );
    assert_original(
        &std::fs::read_to_string(state.backend.integration.manager.config_path()).unwrap(),
    );

    server
        .shutdown_with_handoff(false, || {
            let before = crate::tests::flushed_journal(directory.path(), &state);
            assert!(
                before
                    .iter()
                    .any(|record| record["event"] == "shutdown_start")
            );
            assert!(!before.iter().any(|record| {
                record["event"] == "session_exit_restore" || record["event"] == "shutdown_complete"
            }));
            let (writing, resume) = hold_journal_event(&state, "session_exit_restore");
            let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
            let returned = callback.begin(deadline, "exit callback confirmed its receipt");
            writing.recv_timeout(deadline.remaining()).unwrap();
            assert!(state.backend.diagnostics.journal.flush_pending_for_test());
            assert!(
                !crate::tests::live_journal(directory.path())
                    .iter()
                    .any(|record| record["event"] == "session_exit_restore")
            );
            resume.send(()).unwrap();
            assert!(returned.wait().unwrap());
            drop(callback);
        })
        .unwrap();

    let records = crate::tests::live_journal(directory.path());
    let receipts: Vec<_> = records
        .iter()
        .filter(|record| {
            record["event"] == "session_exit_restore" || record["event"] == "shutdown_complete"
        })
        .collect();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0]["event"], "session_exit_restore");
    assert_eq!(receipts[0]["fields"]["success"], true);
    assert_eq!(receipts[1]["event"], "shutdown_complete");
    assert!(receipts[0]["sequence"].as_u64().unwrap() < receipts[1]["sequence"].as_u64().unwrap());
}

#[test]
fn final_journal_flush_does_not_hold_the_service_lock() {
    let (directory, server) = fixture();
    let state = Arc::clone(&server.state);
    let shutdown = Observation::spawn(move |probe| {
        probe.observe(|_| server.shutdown());
    });
    let (writing, resume) = hold_journal_event(&state, "shutdown_complete");
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let result = shutdown.begin(deadline, "finish shutdown");
    writing.recv_timeout(deadline.remaining()).unwrap();
    let _new_owner = emp_state::IntegrationFileLock::acquire(
        &directory
            .path()
            .canonicalize()
            .unwrap()
            .join("state/service.lock"),
        Duration::ZERO,
        Duration::ZERO,
    )
    .expect("service ownership is available before final journal acknowledgement");
    resume.send(()).unwrap();
    result.wait().unwrap().unwrap();
}

#[test]
fn exit_unwind_restores_before_the_process_finishes() {
    let (_directory, server) = fixture();
    let state = Arc::clone(&server.state);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _restore = RestoreBeforeExit::new(Arc::clone(&state));
        panic!("synthetic startup failure");
    }));
    assert!(result.is_err());
    assert_original(
        &std::fs::read_to_string(state.backend.integration.manager.config_path()).unwrap(),
    );
    assert!(state.lifecycle.shutdown.load(Ordering::Acquire));
    server.shutdown().unwrap();
}

#[test]
fn exit_restores_a_written_lease_before_enable_records_completion() {
    let (_directory, server) = fixture();
    let state = Arc::clone(&server.state);
    state
        .backend
        .integration
        .owned
        .store(false, Ordering::Release);
    server.shutdown().unwrap();
    assert_original(
        &std::fs::read_to_string(state.backend.integration.manager.config_path()).unwrap(),
    );
}

#[test]
fn exit_removes_routing_even_when_the_user_edited_search_settings() {
    let (_directory, server) = fixture();
    let manager = &server.state.backend.integration.manager;
    let applied = std::fs::read_to_string(manager.config_path()).unwrap();
    std::fs::write(
        manager.config_path(),
        applied.replace("web_search = \"live\"", "web_search = \"disabled\""),
    )
    .unwrap();
    assert!(matches!(
        prepare_exit(&server.state, false),
        Err(AppError::NativeRestoreBlocked(
            "search_configuration_restore_failed"
        ))
    ));
    let restored = std::fs::read_to_string(manager.config_path()).unwrap();
    assert!(restored.contains("https://native.example/v1"));
    assert!(!restored.contains(&server.state.base_url));
    assert!(!restored.contains("synthetic-models.json"));
    assert!(restored.contains("web_search = \"disabled\""));
    assert!(server.shutdown().is_err());
}

#[test]
fn exit_never_restores_a_lease_reassigned_to_another_instance() {
    let (_directory, server) = fixture();
    let manager = &server.state.backend.integration.manager;
    let mut other = manager.clone();
    other.instance_id = "another-instance".to_owned();
    assert!(other.recover(true, true).unwrap().ok());
    let config = std::fs::read(manager.config_path()).unwrap();
    let search = std::fs::read(manager.lease_path().with_file_name("search.json")).unwrap();
    assert!(matches!(
        prepare_exit(&server.state, false),
        Err(AppError::NativeRestoreBlocked("integration_owner_changed"))
    ));
    assert_eq!(std::fs::read(manager.config_path()).unwrap(), config);
    assert_eq!(
        std::fs::read(manager.lease_path().with_file_name("search.json")).unwrap(),
        search
    );
    assert!(server.shutdown().is_err());
}

#[test]
fn exit_keeps_a_user_catalog_edit_but_removes_the_owned_routing_overrides() {
    let (_directory, server) = fixture();
    let manager = &server.state.backend.integration.manager;
    let applied = std::fs::read_to_string(manager.config_path()).unwrap();
    std::fs::write(
        manager.config_path(),
        applied.replace("synthetic-models.json", "user-models.json"),
    )
    .unwrap();
    assert!(matches!(
        prepare_exit(&server.state, false),
        Err(AppError::NativeRestoreBlocked(
            "native_configuration_conflict"
        ))
    ));
    let restored = std::fs::read_to_string(manager.config_path()).unwrap();
    assert!(restored.contains("https://native.example/v1"));
    assert!(restored.contains("user-models.json"));
    assert!(!restored.contains(&server.state.base_url));
    assert!(!restored.contains("experimental_realtime_ws_base_url"));
    let status = manager.status().unwrap();
    assert_eq!(status.state, "conflict");
    assert!(status.conflicts.contains(&"model_catalog_json".to_owned()));
    assert!(server.shutdown().is_err());
}

#[test]
fn exit_serializes_an_admitted_enable_and_rejects_later_activation() {
    let (_directory, server) = fixture();
    let state = Arc::clone(&server.state);
    let operation = state.backend.integration.manager.operation_lock().unwrap();
    state
        .backend
        .integration
        .owned
        .store(false, Ordering::Release);
    let exit_state = Arc::clone(&state);
    let exit = std::thread::spawn(move || prepare_exit(&exit_state, false));
    let deadline = Deadline::after(test_timeout(Duration::from_secs(1)));
    while !state.lifecycle.shutdown.load(Ordering::Acquire) {
        assert!(!deadline.is_expired());
        std::thread::yield_now();
    }
    deadline.accept((), "exit closed admission").unwrap();
    // Finish an enable that owned the operation lock before the exit event.
    state
        .backend
        .integration
        .owned
        .store(true, Ordering::Release);
    drop(operation);
    exit.join().unwrap().unwrap();
    let before = std::fs::read(state.backend.integration.manager.config_path()).unwrap();
    let reply = crate::services::integration::commands::execute(None, &state, "enable", true, 0);
    assert_eq!(reply.status, 503);
    crate::services::startup::reconcile(&state).unwrap();
    assert_eq!(
        std::fs::read(state.backend.integration.manager.config_path()).unwrap(),
        before
    );
    assert_original(&String::from_utf8(before).unwrap());
    server.shutdown().unwrap();
}
