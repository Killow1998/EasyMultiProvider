//! Online catalog discovery using a real Codex with synthetic subscription credentials.
use super::installed_codex::Codex;
use super::*;

#[test]
#[ignore = "requires an explicitly selected Codex and isolated loopback access"]
fn installed_codex_refreshes_online_catalog_without_process_restart() {
    online_catalog(false);
}

#[test]
#[ignore = "requires an explicitly selected Codex and isolated loopback access"]
fn installed_codex_uses_localhost_with_a_localhost_only_proxy_exception() {
    online_catalog(true);
}

fn online_catalog(localhost_only: bool) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let home = root.join("codex");
    std::fs::create_dir(&home).unwrap();
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let claims = json!({"email":"catalog@example.test", "exp":2100000000,
        "https://api.openai.com/auth":{"chatgpt_plan_type":"plus",
            "chatgpt_user_id":"fixture-user","chatgpt_account_id":"fixture-account"}});
    let token = format!(
        "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{}.c2ln",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    std::fs::write(
        home.join("auth.json"),
        serde_json::to_vec(&json!({
        "auth_mode":"chatgpt", "tokens":{"id_token":token,"access_token":"fixture-only",
        "refresh_token":"","account_id":"fixture-account"},
        "last_refresh":"2026-10-12T00:00:00Z"}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "model = \"demo/model\"\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\nresponses_websockets = true\nresponses_websockets_v2 = true\n").unwrap();
    let upstream = OneShotUpstream::start(
        json!({"id":"online-catalog","object":"response","status":"completed","model":"upstream",
        "output":[{"id":"msg_online","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]}],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}),
    );
    let config_path = root.join("config.json");
    std::fs::write(&config_path, serde_json::to_vec(&json!({
        "providers":[{"id":"demo","base_url":upstream.base_url(),"protocol":"responses","api_key":"fixture-only"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream","context_window":100000,"enabled":true}]
    })).unwrap()).unwrap();
    let binary = std::env::var("EMP_TEST_CODEX_CLI").expect("explicit installed engine");
    let version = std::process::Command::new(&binary)
        .arg("--version")
        .env("CODEX_HOME", &home)
        .env("HOME", &home)
        .output()
        .expect("selected Codex version");
    assert!(version.status.success());
    let version = String::from_utf8(version.stdout).unwrap();
    let version = version.strip_prefix("codex-cli ").unwrap().trim();
    assert!(!version.is_empty());
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        &binary,
        home.join("auth.json"),
    )
    .unwrap();
    assert!(
        crate::services::integration::enable::apply(&server.state)
            .unwrap_or_else(|_| panic!("apply online integration"))
            .ok()
    );
    assert!(
        !std::fs::read_to_string(home.join("config.toml"))
            .unwrap()
            .contains("model_catalog_json")
    );
    let mut codex = if localhost_only {
        let manager = &server.state.backend.integration.manager;
        assert!(manager.restore().unwrap().ok());
        let endpoint = format!("http://localhost:{}/v1", server.local_addr().port());
        assert!(
            manager
                .enable_with_sideband(&endpoint, None, true, Some(&endpoint))
                .unwrap()
                .ok()
        );
        Codex::start_with_no_proxy(&home, "localhost")
    } else {
        Codex::start(&home)
    };
    let initialized = codex.rpc(1,"initialize",json!({"clientInfo":{"name":"online_catalog_fixture","version":"1"},"capabilities":{"experimentalApi":true}}));
    assert!(initialized["userAgent"].as_str().unwrap().contains(version));
    codex.send(json!({"method":"initialized"}));
    let first = codex.rpc(2, "model/list", json!({"includeHidden":true}));
    assert!(
        first["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "demo/model"),
        "{first}"
    );
    {
        let mut config = server.state.backend.configuration.snapshot().unwrap();
        let mut added = config["models"][0].clone();
        added["id"] = json!("demo/new-model");
        added["upstream_id"] = json!("new-upstream");
        added["context_window"] = json!(600000);
        config["models"].as_array_mut().unwrap().push(added);
        let updated = post(
            &server,
            "/api/config",
            &serde_json::to_vec(&config).unwrap(),
            &[&session_header(&server)],
        );
        assert!(updated.starts_with("HTTP/1.1 200"), "{updated}");
    }
    crate::services::catalog::refresh_catalog(&server.state).unwrap();
    let thread = codex.rpc(3,"thread/start",json!({"model":"demo/model","cwd":home,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true}));
    let id = thread["thread"]["id"].as_str().unwrap();
    codex.rpc(
        4,
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":"Return ok."}]}),
    );
    codex.wait(|event| event["method"] == "turn/completed");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut rpc_id = 5;
    loop {
        let models = codex.rpc(rpc_id, "model/list", json!({"includeHidden":true}));
        if models["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "demo/new-model")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "running engine did not load new online model: {models}"
        );
        rpc_id += 1;
        thread::sleep(Duration::from_millis(100));
    }
    drop(codex);
    server.shutdown().unwrap();
}
