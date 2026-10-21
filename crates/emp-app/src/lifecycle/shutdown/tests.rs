use super::*;
use std::sync::atomic::Ordering;
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
    let directory = tempfile::tempdir().unwrap();
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
            .enable_with_sideband(
                &server.state.base_url,
                Some("synthetic-models.json"),
                true,
                Some(&server.state.base_url),
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
    let request = state.connection_admission.acquire_request().unwrap();
    let websocket = state.connection_admission.acquire_websocket().unwrap();
    let start = Instant::now();
    let shutdown = std::thread::spawn(move || server.shutdown());
    while state.backend.integration.manager.status().unwrap().state != "restored" {
        assert!(start.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_original(
        &std::fs::read_to_string(state.backend.integration.manager.config_path()).unwrap(),
    );
    assert!(state.connection_admission.acquire_request().is_none());
    assert!(state.connection_admission.acquire_websocket().is_none());
    assert!(
        !shutdown.is_finished(),
        "exit waits for admitted handlers after restoring"
    );
    drop((request, websocket));
    shutdown.join().unwrap().unwrap();
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
    assert!(state.shutdown.load(Ordering::Acquire));
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
    let deadline = Instant::now() + Duration::from_secs(1);
    while !state.shutdown.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
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
