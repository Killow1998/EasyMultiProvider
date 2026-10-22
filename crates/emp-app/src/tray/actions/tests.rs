use super::*;
use crate::lifecycle::ServerHandle;
use std::time::Instant;

#[test]
fn tray_restore_and_quit_wait_for_every_admitted_request() {
    for action in [Action::Restore, Action::Quit] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let codex = root.join("codex");
        std::fs::create_dir(&codex).unwrap();
        let original = "# user settings\nmodel = \"fixture-native\"\n";
        std::fs::write(codex.join("config.toml"), original).unwrap();
        let server = ServerHandle::start_with_config_options(
            "127.0.0.1".parse().unwrap(),
            0,
            &root.join("emp.json"),
            "missing-test-codex",
            codex.join("auth.json"),
        )
        .unwrap();
        let state = Arc::clone(&server.state);
        let mut changes = state.backend.management_events.subscribe().unwrap();
        changes.next(Duration::ZERO).unwrap();
        assert!(
            state
                .backend
                .integration
                .manager
                .enable(&state.base_url, None, true)
                .unwrap()
                .ok()
        );
        state
            .backend
            .integration
            .owned
            .store(true, Ordering::Release);
        let admitted = state.connection_admission.acquire_request().unwrap();
        let waiting = Arc::clone(&state);
        let (actions, receive) = mpsc::sync_channel(4);
        let (updates, observed) = mpsc::sync_channel(8);
        let worker = std::thread::spawn(move || run(waiting, receive, updates));
        actions.send(action).unwrap();
        let busy = loop {
            let snapshot = observed.recv_timeout(Duration::from_secs(2)).unwrap();
            if snapshot.busy == Some(action) {
                break snapshot;
            }
        };
        assert!(busy.toggle().is_none());
        assert_eq!(
            if action == Action::Quit {
                busy.quit_label()
            } else {
                busy.toggle_label()
            },
            if action == Action::Quit {
                "正在退出 EMP…"
            } else {
                "正在恢复原生…"
            }
        );
        assert!(busy.tooltip().contains("历史"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.connection_admission.acquire_request().is_some() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !worker.is_finished(),
            "The tray has no management request to exempt"
        );
        assert_eq!(
            state.backend.integration.manager.status().unwrap().state,
            "active"
        );
        assert!(!state.shutdown.load(Ordering::Acquire));
        drop(admitted);
        let ready = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(ready.busy, None);
        drop(actions);
        worker.join().unwrap();
        if action == Action::Restore {
            assert!(changes.next(Duration::ZERO).unwrap().integration);
        }
        assert!(matches!(
            state
                .backend
                .integration
                .manager
                .status()
                .unwrap()
                .state
                .as_str(),
            "native" | "restored"
        ));
        assert_eq!(
            state.shutdown.load(Ordering::Acquire),
            action == Action::Quit
        );
        assert!(
            !std::fs::read_to_string(codex.join("config.toml"))
                .unwrap()
                .contains(&state.base_url)
        );
        server.shutdown().unwrap();
    }
}
