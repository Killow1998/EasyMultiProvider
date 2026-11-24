use crate::support::{
    self, Emp, NATIVE_TOKEN, PROVIDER_KEY, Upstream, Workspace, emp_command, write_file,
};
use emp_test_support::{Deadline, test_timeout};
use serde_json::{Value, json};

#[test]
fn cli_reports_help() {
    for arguments in [&["--help"][..], &["serve", "--help"], &["doctor", "--help"]] {
        let help = emp_command().args(arguments).output().expect("run EMP");
        assert!(help.status.success(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&help.stdout).contains("usage:"),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&help.stdout)
        );
    }
}

#[test]
fn browser_bootstrap_is_one_use_and_unlocks_management_without_secrets() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    assert!(
        emp.startup_output
            .iter()
            .any(|line| *line == format!("Configuration file: {}", workspace.config_path.display())),
        "{:?}",
        emp.startup_output
    );

    let page = emp.anonymous("GET", "/");
    assert_eq!(page.status, 200);
    assert_eq!(page.body, include_bytes!("../../web/index.html").as_slice());

    let replay = support::http(
        emp.port,
        "POST",
        "/api/session",
        &[("X-EMP-Bootstrap", &emp.bootstrap)],
        b"",
    );
    assert_eq!(replay.status, 401, "bootstrap tokens are single use");
    assert_eq!(emp.anonymous("GET", "/api/config").status, 401);

    let config = emp.get("/api/config");
    assert_eq!(config.status, 200, "{}", config.text());
    assert!(!config.text().contains(PROVIDER_KEY));
    assert!(!config.text().contains(NATIVE_TOKEN));
    let config = config.json();
    assert_eq!(
        config["port"], emp.port,
        "publish the actual bound listener port"
    );
    assert_eq!(config["models"].as_array().expect("models").len(), 4);
    assert_eq!(config["emp_version"], env!("CARGO_PKG_VERSION"));
    upstream.assert_idle();
}

#[test]
fn support_report_is_authenticated_downloadable_and_private() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    assert_eq!(emp.anonymous("GET", "/api/support-report").status, 401);

    let response = emp.get("/api/support-report");
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(
        response.header("content-disposition"),
        Some("attachment; filename=\"EMP-support-report.json\"")
    );
    let text = response.text();
    for private in [
        PROVIDER_KEY,
        NATIVE_TOKEN,
        "fixture-account",
        workspace.root.to_str().expect("UTF-8 root"),
    ] {
        assert!(!text.contains(private), "support report leaked {private}");
    }
    let report = response.json();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["emp_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["configuration"]["location"], "custom");
    assert_eq!(report["configuration"]["path"], "<custom>/config.json");
    assert_eq!(report["configuration"]["exists"], true);
    assert_eq!(report["network"]["connectivity_probe"], "not_run");
    assert_eq!(report["accounts"]["imported_count"], 0);
    // The value reflects EMP's own background quota check (which depends on
    // whether a Codex binary exists on this machine), not the report.
    assert!(report["accounts"]["native"]["quota_status"].is_string());
    assert!(
        report["codex"]["inventory"]
            .as_array()
            .expect("inventory")
            .len()
            <= 16
    );
    // Producing the report must not refresh quota or contact any provider.
    upstream.assert_idle();
}

#[test]
fn second_service_for_the_same_configuration_is_rejected() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let before = std::fs::read(&workspace.config_path).expect("config");

    let contender = workspace
        .command()
        .args(workspace.serve_arguments())
        .output()
        .expect("run contender");
    assert_eq!(contender.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&contender.stderr)
            .contains("another EMP service owns this configuration"),
        "{}",
        String::from_utf8_lossy(&contender.stderr)
    );
    assert_eq!(
        std::fs::read(&workspace.config_path).expect("config"),
        before
    );
    assert_eq!(emp.anonymous("GET", "/healthz").status, 200);
}

#[test]
fn discovered_models_import_and_survive_a_restart() {
    let upstream = Upstream::start();
    let workspace = Workspace::new();
    // A pasted endpoint URL is normalized back to the provider's /v1 root.
    workspace.write_config(json!({
        "providers": [{"id": "demo", "base_url": format!("{}/responses/", upstream.base_url()),
                       "protocol": "responses", "auth_mode": "api_key",
                       "api_key": PROVIDER_KEY}],
        "models": [],
    }));
    let advertised = json!({"data": [
        {"id": "vision", "name": "Vision", "created_at": "2026-01-01",
         "architecture": {"input_modalities": ["text", "image"],
                          "output_modalities": ["text", "audio"]}},
        {"id": "audio", "name": "Audio", "created_at": "2026-09-20T00:00:00Z",
         "architecture": {"input_modalities": ["audio"]}},
    ]});
    let emp = workspace.start();

    upstream.reply_json(&advertised);
    let preview = emp.post("/api/providers/discover", &json!({"provider": "demo"}));
    assert_eq!(preview.status, 200, "{}", preview.text());
    let request = upstream.next_request();
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("GET", "/v1/models")
    );
    let preview = preview.json();
    let previewed: Vec<(Value, Value, Value)> = preview["models"]
        .as_array()
        .expect("preview models")
        .iter()
        .map(|model| {
            (
                model["upstream_id"].clone(),
                model["created_at"].clone(),
                model["input_modalities"].clone(),
            )
        })
        .collect();
    // Date-only values are midnight UTC; full timestamps keep their instant.
    assert_eq!(
        previewed,
        [
            (
                json!("vision"),
                json!(1_767_225_600),
                json!(["text", "image"])
            ),
            (json!("audio"), json!(1_789_862_400), json!(["audio"])),
        ]
    );
    // Preview alone changes nothing.
    assert_eq!(emp.get("/api/config").json()["models"], json!([]));

    upstream.reply_json(&advertised);
    let imported = emp.post(
        "/api/providers/discover",
        &json!({"provider": "demo", "selected": ["vision", "audio"]}),
    );
    assert_eq!(imported.status, 200, "{}", imported.text());
    assert_eq!(imported.json()["added"], 2);
    upstream.next_request();
    emp.stop();

    let restarted = workspace.start();
    let config = restarted.get("/api/config").json();
    assert_eq!(config["providers"][0]["base_url"], upstream.base_url());
    let saved: Vec<(Value, Value)> = config["models"]
        .as_array()
        .expect("saved models")
        .iter()
        .map(|model| {
            (
                model["created_at"].clone(),
                model["input_modalities"].clone(),
            )
        })
        .collect();
    assert_eq!(
        saved,
        [
            (json!(1_767_225_600), json!(["text", "image"])),
            (json!(1_789_862_400), json!(["audio"])),
        ]
    );
    let catalog = restarted.codex("GET", "/v1/models?client_version=0.156.1", None);
    assert_eq!(catalog.status, 200, "{}", catalog.text());
    assert_eq!(
        catalog.json()["models"].as_array().expect("models").len(),
        2,
        "imported models are offered to Codex after restart"
    );
    upstream.assert_idle();
}

#[test]
fn desktop_launch_opens_the_bootstrap_url_on_the_configured_port() {
    let workspace = Workspace::new();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve port")
        .local_addr()
        .expect("address")
        .port();
    // The per-user desktop location: XDG on Linux, Application Support on macOS.
    let config_path = if cfg!(target_os = "macos") {
        workspace
            .root
            .join("home/Library/Application Support/EasyMultiProvider/config.json")
    } else {
        workspace.root.join("xdg/easy-multi-provider/config.json")
    };
    write_file(
        &config_path,
        &json!({"host": "127.0.0.1", "port": port,
                "native_catalog_path": workspace.root.join("native.json")})
        .to_string(),
    );
    let opened = workspace.root.join("browser-url.txt");
    let browser = workspace.root.join("browser");
    write_file(
        &browser,
        // Write then rename so the test never observes a partial URL.
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > '{0}.part' && mv '{0}.part' '{0}'\n",
            opened.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o700))
            .expect("browser mode");
    }
    let mut command = workspace.command();
    command.env("BROWSER", &browser);
    let emp = Emp::spawn(command);
    assert_eq!(emp.port, port);
    assert_eq!(emp.anonymous("GET", "/healthz").status, 200);
    assert!(
        emp.startup_output
            .iter()
            .any(|line| *line == format!("Configuration file: {}", config_path.display())),
        "{:?}",
        emp.startup_output
    );

    let deadline = Deadline::after(test_timeout(std::time::Duration::from_secs(5)));
    while !opened.exists() {
        assert!(!deadline.is_expired(), "browser was not launched");
        #[expect(
            clippy::disallowed_methods,
            reason = "浏览器替身通过文件报告启动，以固定间隔轮询"
        )]
        std::thread::sleep(
            deadline
                .remaining()
                .min(std::time::Duration::from_millis(10)),
        );
    }
    deadline.accept((), "browser launched").unwrap();
    let url = std::fs::read_to_string(&opened).expect("opened URL");
    assert!(
        url.starts_with(&format!("http://127.0.0.1:{port}/?bootstrap=")),
        "{url}"
    );
}
