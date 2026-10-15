//! Recovery must work before the normal vault/backend exists.
#[allow(dead_code)]
mod support;
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use support::{Workspace, http};

struct Recovery {
    child: Child,
    lines: Receiver<String>,
    port: u16,
    token: String,
}

impl Recovery {
    fn start(workspace: &Workspace) -> Self {
        let mut command = workspace.command();
        command
            .args(workspace.serve_arguments())
            .env_remove("EASY_MULTI_PROVIDER_MASTER_KEY");
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let mut recovery = Self {
            child,
            lines,
            port: 0,
            token: String::new(),
        };
        let url = recovery.wait_line("Recovery page: ");
        let (base, token) = url.split_once("/#recovery=").unwrap();
        recovery.port = base.rsplit(':').next().unwrap().parse().unwrap();
        recovery.token = token.into();
        recovery
    }

    fn wait_line(&self, prefix: &str) -> String {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(15))
                .expect("process did not reach expected state");
            if let Some(value) = line.strip_prefix(prefix) {
                return value.into();
            }
        }
    }

    fn action(&self, action: &str, confirm: Option<&str>) -> support::Response {
        let mut headers = vec![("X-EMP-Recovery", self.token.as_str())];
        if let Some(confirm) = confirm {
            headers.push(("X-EMP-Recovery-Confirm", confirm));
        }
        http(
            self.port,
            "POST",
            &format!("/api/recovery/{action}"),
            &headers,
            b"",
        )
    }

    fn wait_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "EMP did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn normal_quit(&mut self) {
        let url = self.wait_line("Open in browser: ");
        let (base, bootstrap) = url.split_once("/?bootstrap=").unwrap();
        let port = base.rsplit(':').next().unwrap().parse().unwrap();
        let session = http(
            port,
            "POST",
            "/api/session",
            &[("X-EMP-Bootstrap", bootstrap)],
            b"",
        );
        assert_eq!(session.status, 200);
        let session = session.json()["session"].as_str().unwrap().to_owned();
        assert_eq!(http(port, "GET", "/healthz", &[], b"").status, 200);
        assert_eq!(
            http(
                port,
                "POST",
                "/api/quit",
                &[
                    ("X-EMP-Session", &session),
                    ("Content-Type", "application/json")
                ],
                b"{}"
            )
            .status,
            200
        );
        self.wait_exit();
    }
}

impl Drop for Recovery {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn broken_key() -> Workspace {
    let workspace = Workspace::new();
    workspace.write_config(json!({"auto_enable_on_start":false}));
    let state = workspace.root.join("state");
    emp_state::filesystem::create_private_directory(&state).unwrap();
    emp_state::filesystem::write_new_private_file(
        &state.join("master.key"),
        b"invalid-key-fixture",
    )
    .unwrap();
    workspace
}

#[test]
fn invalid_key_shows_recovery_and_exit_keeps_every_old_file() {
    let workspace = broken_key();
    let config = std::fs::read(&workspace.config_path).unwrap();
    let mut recovery = Recovery::start(&workspace);
    assert_eq!(http(recovery.port, "GET", "/", &[], b"").status, 200);
    assert_eq!(http(recovery.port, "GET", "/healthz", &[], b"").status, 503);
    assert_eq!(
        http(recovery.port, "GET", "/api/recovery", &[], b"").status,
        401
    );
    let auth = [("X-EMP-Recovery", recovery.token.as_str())];
    let state = http(recovery.port, "GET", "/api/recovery", &auth, b"").json();
    assert_eq!(state["reason"], "key_invalid");
    assert_eq!(recovery.action("reset", None).status, 409);
    assert_eq!(
        http(
            recovery.port,
            "POST",
            "/api/recovery/reset",
            &[
                ("X-EMP-Recovery", &recovery.token),
                ("X-EMP-Recovery-Confirm", "clear-and-reset"),
                ("Origin", "https://untrusted.example")
            ],
            b""
        )
        .status,
        403
    );
    assert!(
        !http(recovery.port, "GET", "/", &[], b"")
            .text()
            .contains("id=\"retry\"")
    );
    assert_eq!(recovery.action("exit", None).status, 200);
    recovery.wait_exit();
    assert_eq!(std::fs::read(&workspace.config_path).unwrap(), config);
    assert_eq!(
        std::fs::read(workspace.root.join("state/master.key")).unwrap(),
        b"invalid-key-fixture"
    );
}

#[test]
fn confirmed_reset_with_optional_backup_reaches_normal_ui_in_the_same_process() {
    for keep in [false, true] {
        let workspace = broken_key();
        let original = workspace.read_codex_config();
        let old_config = std::fs::read(&workspace.config_path).unwrap();
        let mut recovery = Recovery::start(&workspace);
        let pid = recovery.child.id();
        let result = recovery.action(
            "reset",
            Some(if keep {
                "backup-and-reset"
            } else {
                "clear-and-reset"
            }),
        );
        assert_eq!(result.status, 200, "{}", result.text());
        let result = result.json();
        if keep {
            let backup = std::path::Path::new(result["backup_path"].as_str().unwrap());
            assert_eq!(
                std::fs::read(backup.join("config.json")).unwrap(),
                old_config
            );
            assert_eq!(
                std::fs::read(backup.join("state/master.key")).unwrap(),
                b"invalid-key-fixture"
            );
        } else {
            assert!(result["backup_path"].is_null());
            assert!(!std::fs::read_dir(&workspace.root).unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("recovery-")
            }));
        }
        recovery.normal_quit();
        assert_eq!(recovery.child.id(), pid);
        assert_eq!(workspace.read_codex_config(), original);
    }
}

#[test]
fn interrupted_reset_can_be_cleared_from_the_page_without_manual_file_repair() {
    let workspace = broken_key();
    let state = workspace.root.join("state");
    let archive = workspace
        .root
        .join("recovery-staging-0123456789abcdef01234567");
    emp_state::filesystem::create_private_directory(&archive.join("state")).unwrap();
    emp_state::filesystem::write_new_private_file(
        &state.join("recovery-reset.json"),
        &serde_json::to_vec(&json!({
            "config_path":workspace.config_path,"saved_files":archive,"keep_backup":false
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::rename(&workspace.config_path, archive.join("config.json")).unwrap();
    std::fs::rename(state.join("master.key"), archive.join("state/master.key")).unwrap();
    let mut recovery = Recovery::start(&workspace);
    let status = http(
        recovery.port,
        "GET",
        "/api/recovery",
        &[("X-EMP-Recovery", &recovery.token)],
        b"",
    )
    .json();
    assert_eq!(status["reason"], "reset_incomplete");
    assert_eq!(
        recovery.action("reset", Some("clear-and-reset")).status,
        200
    );
    recovery.normal_quit();
    assert!(!archive.exists());
    assert!(!state.join("recovery-reset.json").exists());
}

#[test]
fn invalid_configuration_can_be_reset_and_external_key_override_stays_a_developer_error() {
    let workspace = broken_key();
    std::fs::write(&workspace.config_path, b"{broken-config").unwrap();
    let mut recovery = Recovery::start(&workspace);
    let state = http(
        recovery.port,
        "GET",
        "/api/recovery",
        &[("X-EMP-Recovery", &recovery.token)],
        b"",
    )
    .json();
    assert_eq!(state["reason"], "config_invalid");
    assert_eq!(
        recovery.action("reset", Some("clear-and-reset")).status,
        200
    );
    recovery.normal_quit();
    let before = std::fs::read(&workspace.config_path).unwrap();
    let output = workspace
        .command()
        .args(workspace.serve_arguments())
        .env("EASY_MULTI_PROVIDER_MASTER_KEY", "invalid-external-fixture")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("external master key override"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Recovery page:"));
    assert_eq!(std::fs::read(&workspace.config_path).unwrap(), before);
}
