//! Real server contract tests.
use super::*;

#[test]
fn migration_export_and_import_cross_the_authenticated_http_boundary() {
    let source_directory = tempfile::tempdir().unwrap();
    let source_root = canonical_root(&source_directory);
    let source_config = source_root.join("config.json");
    std::fs::write(&source_config, serde_json::to_vec_pretty(&json!({
            "providers":[{"id":"demo","name":"Demo","base_url":"https://api.example.com/v1","protocol":"responses","auth_mode":"api_key","api_key":"secret"}],
            "models":[{"id":"demo/model","provider":"demo","upstream_id":"model","enabled":true}]
        })).unwrap()).unwrap();
    let source = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &source_config,
        "codex",
        source_root.join("auth.json"),
    )
    .unwrap();
    let export = post(
        &source,
        "/api/migration/export",
        br#"{"password":"12345678","groups":["external"]}"#,
        &[&session_cookie_header(&source)],
    );
    assert!(export.starts_with("HTTP/1.1 200 OK\r\n"), "{export}");
    assert!(export.contains("Content-Disposition: attachment; filename=\"EMP.emp\"\r\n"));
    let bundle = export.split_once("\r\n\r\n").unwrap().1.as_bytes();
    assert!(bundle.starts_with(b"EMP-MIGRATION\x01\n"));
    source.shutdown().unwrap();

    let target_directory = tempfile::tempdir().unwrap();
    let target_root = canonical_root(&target_directory);
    let target_config = target_root.join("config.json");
    std::fs::write(&target_config, b"{}").unwrap();
    let target = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &target_config,
        "codex",
        target_root.join("auth.json"),
    )
    .unwrap();
    let import_body = serde_json::to_vec(&json!({
        "password":"12345678",
        "bundle":STANDARD.encode(bundle)
    }))
    .unwrap();
    let imported = post(
        &target,
        "/api/migration/import",
        &import_body,
        &[&session_cookie_header(&target)],
    );
    assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
    let config = request(&target, "/api/config", &[&session_cookie_header(&target)]);
    assert!(config.contains("demo/model"));
    assert!(!config.contains("\"api_key\":\"secret\""));
    let stored = target
        .state
        .backend
        .configuration
        .config
        .lock()
        .unwrap()
        .clone();
    let provider = stored["providers"].as_array().unwrap()[0].clone();
    assert_eq!(
        provider_api_key(&provider, &target.state.backend.configuration.vault),
        "secret"
    );
    target.shutdown().unwrap();
}

#[test]
fn account_import_and_delete_keep_credentials_managed_and_private() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    std::fs::write(&config, b"{}").unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "codex",
        root.join("auth.json"),
    )
    .unwrap();
    let imported = post(
        &server,
        "/api/accounts/import",
        &serde_json::to_vec(&json!({
            "id":"egg","name":"Egg","prefix":"egg","enabled":true,
            "auth_json":{"tokens":{"access_token":"account-secret","account_id":"account-id"}}
        }))
        .unwrap(),
        &[&session_cookie_header(&server)],
    );
    assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
    assert!(imported.contains("\"credential_set\":true"));
    assert!(!imported.contains("account-secret"));
    let stored = server
        .state
        .backend
        .configuration
        .config
        .lock()
        .unwrap()
        .clone();
    let auth_path = PathBuf::from(stored["accounts"][0]["auth_file"].as_str().unwrap());
    assert!(auth_path.is_file());
    assert!(auth_path.parent().unwrap().join("config.toml").is_file());
    assert_eq!(
        server
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&auth_path)
            .unwrap()["tokens"]["access_token"],
        "account-secret"
    );
    let removed = delete(
        &server,
        "/api/accounts/egg",
        &[&session_cookie_header(&server)],
    );
    assert!(removed.starts_with("HTTP/1.1 200 OK\r\n"), "{removed}");
    assert!(!auth_path.exists());
    assert!(!auth_path.parent().unwrap().join("config.toml").exists());
    assert!(
        server.state.backend.configuration.config.lock().unwrap()["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    server.shutdown().unwrap();
}

#[test]
fn native_search_forwards_raw_json_with_the_best_available_login() {
    let upstream = OneShotUpstream::start(json!({"data":[{"title":"result"}]}));
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "codex_base_url":format!("http://{}/backend",upstream.address),
            "subscription_search":{"enabled":true,"account_id":""}
        }))
        .unwrap(),
    )
    .unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "codex",
        root.join("missing-auth.json"),
    )
    .unwrap();
    let response = post(
        &server,
        "/v1/alpha/search",
        br#"{"query":"codex"}"#,
        &[
            &session_cookie_header(&server),
            "Authorization: Bearer caller-token",
            "chatgpt-account-id: caller-account",
        ],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("\"title\":\"result\""));
    let (path, headers, body) = upstream.observed();
    assert_eq!(path, "/backend/alpha/search");
    assert_eq!(headers["authorization"], "Bearer caller-token");
    assert_eq!(headers["chatgpt-account-id"], "caller-account");
    assert_eq!(body, json!({"query":"codex"}));
    server.shutdown().unwrap();
}

#[test]
fn integration_api_applies_and_shutdown_restores_only_owned_codex_fields() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let config = root.join("emp-config.json");
    // Python rejects applying an empty model picker; this success scenario needs
    // the same visible model fixture as tests.test_server._integration_test_config.
    std::fs::write(&config, serde_json::to_vec(&json!({
        "providers":[{"id":"external","base_url":"https://example.invalid/v1","protocol":"responses"}],
        "models":[{"id":"external/model-a","provider":"external","upstream_id":"model-a","enabled":true}]
    })).unwrap()).unwrap();
    let codex_config = root.join("config.toml");
    std::fs::write(
        &codex_config,
        b"# keep\nopenai_base_url = \"native\"\n[features]\nweb_search = true\n",
    )
    .unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let before = request(
        &server,
        "/api/integration",
        &[&session_cookie_header(&server)],
    );
    assert!(before.contains("\"state\":\"native\""), "{before}");
    let enabled = post(
        &server,
        "/api/integration/enable",
        br#"{"confirm_reload":true}"#,
        &[&session_cookie_header(&server)],
    );
    assert!(enabled.starts_with("HTTP/1.1 200 OK\r\n"), "{enabled}");
    assert!(enabled.contains("\"state\":\"emp_applied\""));
    let applied = std::fs::read_to_string(&codex_config).unwrap();
    assert!(applied.contains(&format!(
        "openai_base_url = \"http://127.0.0.1:{}/v1\"",
        server.local_addr().port()
    )));
    assert!(applied.contains("model_catalog_json"));
    assert!(applied.contains("[features]\nweb_search = true"));
    server.shutdown().unwrap();
    let restored = std::fs::read_to_string(&codex_config).unwrap();
    assert!(restored.contains("openai_base_url = \"native\""));
    assert!(!restored.contains("model_catalog_json"));
    assert!(restored.contains("[features]\nweb_search = true"));
}
