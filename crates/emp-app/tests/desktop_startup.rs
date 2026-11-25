//! Actual desktop event-loop startup, with synthetic state and no browser window.
#![cfg(any(windows, target_os = "macos"))]

#[allow(
    dead_code,
    reason = "Desktop targets use a platform-specific subset of shared fixtures"
)]
mod support;

use emp_test_support::{Deadline, test_timeout};
use std::time::Duration;
use support::{Emp, Upstream, Workspace};

#[test]
fn desktop_startup_keeps_the_native_tray_alive_through_enable_and_restore() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&workspace.config_path).unwrap()).unwrap();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    config["host"] = "127.0.0.1".into();
    config["port"] = listener.local_addr().unwrap().port().into();
    config["auto_enable_on_start"] = false.into();
    workspace.write_config(config);
    drop(listener);
    let mut command = workspace.command();
    command
        .env("EASY_MULTI_PROVIDER_CONFIG", &workspace.config_path)
        .env(
            "BROWSER",
            format!("\"{}\" --version", env!("CARGO_BIN_EXE_EMP")),
        );
    let mut emp = Emp::spawn(command);
    let deadline = Deadline::after(test_timeout(Duration::from_secs(15)));
    loop {
        let journal: String = match std::fs::read_dir(workspace.root.join("state/logs")) {
            Ok(entries) => entries
                .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read desktop startup journal: {error}"),
        };
        assert!(!journal.contains("desktop_tray_unavailable"));
        if journal.lines().any(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|record| record["event"] == "desktop_tray_started")
        }) {
            deadline.accept((), "desktop tray startup").unwrap();
            break;
        }
        assert!(!deadline.is_expired(), "native tray did not start");
        #[expect(
            clippy::disallowed_methods,
            reason = "原生托盘就绪通过诊断日志报告，以固定间隔轮询"
        )]
        std::thread::sleep(Duration::from_millis(20));
    }
    for (operation, expected) in [("enable", "emp_applied"), ("restore", "native")] {
        let result = emp.post(
            &format!("/api/integration/{operation}"),
            &serde_json::json!({"confirm_reload":true}),
        );
        assert_eq!(result.status, 200, "{}", result.text());
        assert_eq!(result.json()["configuration"]["state"], expected);
        assert_eq!(emp.anonymous("GET", "/healthz").status, 200);
    }
    assert_eq!(emp.post("/api/quit", &serde_json::json!({})).status, 200);
    assert_eq!(emp.wait_exit(), Some(0));
    let lines = |text: &str| {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        lines(&workspace.read_codex_config()),
        lines(support::ORIGINAL_CODEX_CONFIG)
    );
}
