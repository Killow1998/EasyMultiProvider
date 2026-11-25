//! Online catalog discovery using a real Codex with synthetic subscription credentials.
use super::installed_codex::Codex;
use super::*;
use emp_test_support::{Deadline, OwnedChild, test_timeout};

#[test]
#[ignore = "requires an explicitly selected Codex and isolated loopback access"]
fn installed_codex_refreshes_online_catalog_without_process_restart() {
    online_catalog(false, "online_catalog_fixture", true, "示例服务");
}

#[test]
#[ignore = "requires an explicitly selected Codex and isolated loopback access"]
fn installed_codex_displays_service_name_with_stable_route() {
    for name in ["Example", "床", "示例服务⭐", "模型%服务"] {
        online_catalog(false, "online_catalog_fixture", false, name);
    }
}

#[test]
#[ignore = "requires an explicitly selected Codex and isolated loopback access"]
fn installed_codex_uses_localhost_with_a_localhost_only_proxy_exception() {
    online_catalog(true, "online_catalog_fixture", true, "示例服务");
}

#[test]
#[ignore = "requires Codex 0.165 or later and isolated loopback access"]
fn installed_codex_refreshes_originator_scoped_catalogs() {
    for originator in ["codex_work_cca", "chatgpt_cca"] {
        online_catalog(false, originator, true, "示例服务");
    }
}

fn online_catalog(localhost_only: bool, originator: &str, check_refresh: bool, service_name: &str) {
    let directory = emp_test_support::fixture_dir().unwrap();
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
        "last_refresh":emp_state::observed_at_now()}))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "model = \"demo/model\"\napproval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nplugins = false\napps = false\nmemories = false\nresponses_websockets = true\nresponses_websockets_v2 = true\n").unwrap();
    let upstream = OneShotUpstream::start(
        json!({"id":"online-catalog","object":"response","status":"completed","model":"upstream",
        "output":[{"id":"msg_online","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]}],
        "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}),
    );
    let config_path = root.join("config.json");
    std::fs::write(&config_path, serde_json::to_vec(&json!({
        "providers":[{"id":"demo","name":service_name,"base_url":upstream.base_url(),"protocol":"responses","api_key":"external-fixture"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream","context_window":100000,"enabled":true}]
    })).unwrap()).unwrap();
    let binary = std::env::var("EMP_TEST_CODEX_CLI").expect("explicit installed engine");
    let mut command = std::process::Command::new(&binary);
    command
        .arg("--version")
        .env("CODEX_HOME", &home)
        .env("HOME", &home);
    let version = OwnedChild::output(
        command,
        Deadline::after(test_timeout(Duration::from_secs(15))),
    )
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
                .enable_with_options(&endpoint, None, true, Some(&endpoint), None)
                .unwrap()
                .ok()
        );
        Codex::start_with_no_proxy(&home, "localhost")
    } else {
        Codex::start(&home)
    };
    let initialized = codex.rpc(1,"initialize",json!({"clientInfo":{"name":originator,"version":"1"},"capabilities":{"experimentalApi":true}}));
    assert!(initialized["userAgent"].as_str().unwrap().contains(version));
    codex.send(json!({"method":"initialized"}));
    let first = codex.rpc(2, "model/list", json!({"includeHidden":true}));
    let model = first["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| {
            model["displayName"]
                .as_str()
                .is_some_and(|name| name.ends_with(&format!("{service_name} · model")))
        })
        .expect("readable service name in the installed Codex picker");
    let selected = model["id"].as_str().unwrap();
    assert!(selected.is_ascii(), "WebSocket routing hint: {selected}");
    assert!(
        emp_state::config::resolve_client_model(
            &server.state.backend.configuration.snapshot().unwrap(),
            selected,
        )
        .as_deref()
            == Some("demo/model"),
        "{model}"
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
    let thread = codex.rpc(3,"thread/start",json!({"model":selected,"cwd":home,"approvalPolicy":"never","sandbox":"read-only","ephemeral":true}));
    let id = thread["thread"]["id"].as_str().unwrap();
    codex.rpc(
        4,
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":"Return ok."}]}),
    );
    let message = codex.wait(|event| event["method"] == "item/agentMessage/delta");
    assert_eq!(message["params"]["delta"], "ok");
    let completed = codex.wait(|event| event["method"] == "turn/completed");
    assert_eq!(completed["params"]["turn"]["status"], "completed");
    let (path, headers, body) = upstream.observed();
    assert_eq!(path, "/v1/responses");
    assert_eq!(headers["authorization"], "Bearer external-fixture");
    assert_eq!(body["model"], "upstream");
    let requests = crate::tests::flushed_journal(&root, &server.state)
        .into_iter()
        .filter(|event| event["event"] == "request_started")
        .collect::<Vec<_>>();
    assert!(!requests.is_empty(), "the selected model must reach EMP");
    assert!(
        requests
            .iter()
            .all(|event| event["fields"]["transport"] == "websocket"),
        "{service_name} route must complete without falling back to HTTP: {requests:?}"
    );
    eprintln!("{service_name}: completed using WebSocket");
    if !check_refresh {
        drop(codex);
        server.shutdown().unwrap();
        return;
    }
    let deadline = Deadline::after(test_timeout(Duration::from_secs(300)));
    let mut rpc_id = 5;
    loop {
        let models = if originator == "online_catalog_fixture" {
            codex.rpc_before(
                rpc_id,
                "model/list",
                json!({"includeHidden":true}),
                &deadline.capped(test_timeout(Duration::from_secs(15))),
            )["data"]
                .clone()
        } else {
            let cached = std::fs::read(home.join(format!("models_cache.{originator}.json")))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            cached.unwrap_or_default()["models"].clone()
        };
        if models.as_array().is_some_and(|entries| {
            entries.iter().any(|model| {
                model[if originator == "online_catalog_fixture" {
                    "id"
                } else {
                    "slug"
                }]
                .as_str()
                .is_some_and(|id| {
                    emp_state::config::resolve_client_model(
                        &server.state.backend.configuration.snapshot().unwrap(),
                        id,
                    )
                    .as_deref()
                        == Some("demo/new-model")
                })
            })
        }) {
            deadline
                .accept((), format_args!("refreshed catalog {originator}"))
                .unwrap();
            break;
        }
        assert!(
            !deadline.is_expired(),
            "running engine did not refresh catalog {originator}; model IDs: {:?}; stderr: {}",
            models
                .as_array()
                .map(|entries| entries.iter().map(|model| &model["id"]).collect::<Vec<_>>()),
            std::fs::read_to_string(home.join("stderr.log")).unwrap(),
        );
        rpc_id += 1;
        #[expect(
            clippy::disallowed_methods,
            reason = "真实 Codex 目录刷新按既有固定间隔观察缓存变化"
        )]
        thread::sleep(Duration::from_millis(500));
    }
    drop(codex);
    server.shutdown().unwrap();
}
