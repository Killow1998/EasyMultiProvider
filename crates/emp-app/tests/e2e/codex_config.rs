use crate::support::{ORIGINAL_CODEX_CONFIG, Upstream, Workspace, write_file};
use emp_test_support::{Deadline, test_timeout};
use serde_json::{Value, json};

#[test]
fn codex_integration_enable_and_restore_preserve_user_toml() {
    // Instructions containing managed-looking lines are string data.
    let original = "# keep this header\n\
                    \"openai_base_url\"   = \"native\"  # keep this inline comment\n\
                    instructions = '''Read this example literally:\n\
                    openai_base_url = \"this is instruction text\"\n\
                    [example]\nend of instructions'''\n\
                    \n[nested]\nopenai_base_url = \"nested-value\"\nenabled = true\n";
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    write_file(&workspace.codex_config(), original);
    let mut emp = workspace.start();
    let pid = emp.pid();

    let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(enabled.status, 200, "{}", enabled.text());
    let applied = workspace.read_codex_config();
    assert!(
        applied.contains(&format!(
            "\"http://{}:{}/v1\"",
            if cfg!(windows) {
                "localhost"
            } else {
                "127.0.0.1"
            },
            emp.port
        )),
        "{applied}"
    );
    assert!(applied.contains("openai_base_url = \"this is instruction text\""));
    assert!(applied.contains("[nested]\nopenai_base_url = \"nested-value\"\nenabled = true"));
    assert!(workspace.integration_dir().join("lease.json").is_file());
    let summary = emp.get("/api/integration");
    assert_eq!(summary.status, 200, "{}", summary.text());
    assert_eq!(summary.json()["configuration"]["state"], "emp_applied");

    let restored = emp.post("/api/integration/restore", &json!({"confirm_reload": true}));
    assert_eq!(restored.status, 200, "{}", restored.text());
    assert_eq!(workspace.read_codex_config(), original);
    assert_eq!(emp.get("/healthz").status, 200);
    assert_eq!(emp.get("/api/config").status, 200);
    assert_eq!(
        emp.get("/api/integration").json()["configuration"]["state"],
        "native"
    );
    // Restore is repeatable and the same running service can be enabled again.
    assert_eq!(
        emp.post("/api/integration/restore", &json!({"confirm_reload":true}))
            .status,
        200
    );
    assert_eq!(
        emp.post("/api/integration/enable", &json!({"confirm_reload":true}))
            .status,
        200
    );
    assert_eq!(
        emp.get("/api/integration").json()["configuration"]["state"],
        "emp_applied"
    );
    assert_eq!(emp.pid(), pid);
    // Only the separate Quit action restores and ends this process.
    assert_eq!(emp.post("/api/quit", &json!({})).status, 200);
    assert_eq!(
        emp.wait_exit_before(&Deadline::after(test_timeout(
            std::time::Duration::from_secs(10)
        ))),
        Some(0)
    );
    assert_eq!(workspace.read_codex_config(), original);
    upstream.assert_idle();
}

#[test]
fn empty_model_picker_cannot_enable_integration() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    // Exercise a rejected manual activation from Native. With automatic
    // activation enabled, the initial non-empty catalog already owns a lease.
    let mut initial: Value =
        serde_json::from_slice(&std::fs::read(&workspace.config_path).unwrap()).unwrap();
    initial["auto_enable_on_start"] = json!(false);
    workspace.write_config(initial);
    let emp = workspace.start();
    assert_eq!(workspace.read_codex_config(), ORIGINAL_CODEX_CONFIG);
    let mut config = emp.get("/api/config").json();
    config["models"] = json!([]);
    let saved = emp.post("/api/config", &config);
    assert_eq!(saved.status, 200, "{}", saved.text());

    let response = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(response.status, 409, "{}", response.text());
    assert_eq!(response.json()["error"]["code"], "empty_emp_catalog");
    assert_eq!(workspace.read_codex_config(), ORIGINAL_CODEX_CONFIG);
    assert!(!workspace.integration_dir().join("lease.json").exists());
}

#[test]
fn subscription_search_enable_restore_and_external_edit_conflict() {
    let original = "# user preferences\nmodel = \"native\"\n[features]\nunified_exec = true\n";
    for edited in [false, true] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        write_file(&workspace.codex_config(), original);
        let emp = workspace.start();
        let mut config = emp.get("/api/config").json();
        config["subscription_search"] = json!({"enabled": true});
        let saved = emp.post("/api/config", &config);
        assert_eq!(saved.status, 200, "{}", saved.text());

        let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
        assert_eq!(enabled.status, 200, "{}", enabled.text());
        let applied = workspace.read_codex_config();
        assert!(applied.contains("web_search = \"live\""), "{applied}");
        assert!(
            applied.contains("standalone_web_search = true"),
            "{applied}"
        );
        assert!(applied.contains("unified_exec = true"), "{applied}");
        let lease_path = workspace.integration_dir().join("search.json");
        let lease: Value =
            serde_json::from_slice(&std::fs::read(&lease_path).expect("search lease"))
                .expect("lease JSON");
        assert_eq!(lease["config_path"], json!(workspace.codex_config()));

        if edited {
            let external = applied.replace("web_search = \"live\"", "web_search = \"disabled\"");
            write_file(&workspace.codex_config(), &external);
            let response = emp.post("/api/integration/restore", &json!({"confirm_reload": true}));
            assert_eq!(response.status, 409, "{}", response.text());
            assert_eq!(
                workspace.read_codex_config(),
                external,
                "a user's own edit is never overwritten"
            );
        } else {
            let response = emp.post("/api/integration/restore", &json!({"confirm_reload": true}));
            assert_eq!(response.status, 200, "{}", response.text());
            let restored = workspace.read_codex_config();
            assert!(!restored.contains("web_search"), "{restored}");
            assert!(restored.contains("unified_exec = true"), "{restored}");
            let lease: Value =
                serde_json::from_slice(&std::fs::read(&lease_path).expect("search lease"))
                    .expect("lease JSON");
            assert_eq!(lease["status"], "restored");
        }
    }
}

#[test]
fn sigterm_restores_the_owned_codex_integration() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut emp = workspace.start();
    let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(enabled.status, 200, "{}", enabled.text());
    assert!(
        workspace
            .read_codex_config()
            .contains(&format!("http://127.0.0.1:{}/v1", emp.port))
    );

    emp.signal(libc::SIGTERM);
    assert_eq!(emp.wait_exit(), Some(0));
    // The user's settings and comments come back; blank-line layout may differ.
    let content_lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    };
    let restored = workspace.read_codex_config();
    assert_eq!(
        content_lines(&restored),
        content_lines(ORIGINAL_CODEX_CONFIG),
        "{restored}"
    );
    assert!(!restored.contains("127.0.0.1"), "{restored}");
}

#[test]
fn quit_from_the_page_stops_the_process() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut emp = workspace.start();
    let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(enabled.status, 200, "{}", enabled.text());
    assert!(workspace.read_codex_config().contains(&format!(
        "http://{}:{}/v1",
        if cfg!(windows) {
            "localhost"
        } else {
            "127.0.0.1"
        },
        emp.port
    )));
    let response = emp.post("/api/quit", &json!({}));
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"status": "stopping"}));
    assert_eq!(
        emp.wait_exit_before(&Deadline::after(test_timeout(
            std::time::Duration::from_secs(30)
        ))),
        Some(0)
    );
    let restored = workspace.read_codex_config();
    let content_lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        content_lines(&restored),
        content_lines(ORIGINAL_CODEX_CONFIG)
    );
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&workspace.config_path).unwrap()).unwrap();
    config["auto_enable_on_start"] = json!(false);
    workspace.write_config(config);
    let mut restarted = workspace.start();
    assert_eq!(restarted.get("/api/config").status, 200);
    assert_eq!(workspace.read_codex_config(), restored);
    assert_eq!(restarted.post("/api/quit", &json!({})).status, 200);
    assert_eq!(
        restarted.wait_exit_before(&Deadline::after(test_timeout(
            std::time::Duration::from_secs(30)
        ))),
        Some(0)
    );
    upstream.assert_idle();
}
