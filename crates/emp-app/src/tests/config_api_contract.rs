use super::catalog_api_contract::{CatalogUpstream, catalog_server, parsed_body};
use super::*;

#[test]
fn settings_save_rejects_invalid_state_and_preserves_credentials_across_restart() {
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let initial = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .expect("config")
        .clone();
    let cookie = session_header(&server);
    let mut valid = parsed_body(&request(&server, "/api/config", &[&cookie]));
    valid["native_model_context_windows"] = json!({"native":150000});
    valid["providers"][0]["name"] = json!("Edited provider");
    valid["models"][0]["input_modalities"] = json!(["text", "image"]);
    valid["models"][0]["context_window"] = json!(80000);
    valid["catalog_family_presentations"] =
        json!({"old":{"catalog_alias":"Daily","show_context":false}});
    valid["codex_runtime_sources"] = json!(["vscode"]);
    valid["native_catalog_path"] = json!("user-form-cannot-replace-managed-path");
    let mut excessive = valid.clone();
    excessive["native_model_context_windows"] = json!({"native":200001});
    let mut invalid_type = valid.clone();
    invalid_type["native_model_context_windows"] = json!({"native":true});
    let mut private_file = valid.clone();
    private_file["providers"][0]["api_key_file"] = json!("form-injected-secret.key");
    let mut restored = valid.clone();
    restored["native_model_context_windows"] = json!({});
    let cases = [valid, excessive, invalid_type, private_file, restored];
    let mut actual = Vec::new();
    for (index, incoming) in cases.iter().enumerate() {
        let before =
            std::fs::read(&server.state.backend.configuration.config_path).expect("before");
        let wire = post(
            &server,
            "/api/config",
            &serde_json::to_vec(incoming).expect("request JSON"),
            &[&cookie],
        );
        let status: u16 = wire
            .split_whitespace()
            .nth(1)
            .expect("status")
            .parse()
            .expect("status number");
        assert_eq!(
            status,
            if index == 0 || index == 4 { 200 } else { 400 },
            "{wire}"
        );
        if status == 400 {
            assert_eq!(
                std::fs::read(&server.state.backend.configuration.config_path).expect("after"),
                before
            );
        }
        assert!(!wire.contains("synthetic-test-key"));
        actual.push(json!({"status":status,"payload":parsed_body(&wire)}));
    }
    let saved = load_configuration(Some(&root.join("config.json"))).expect("saved config");
    assert_eq!(
        saved["codex_runtime_sources"],
        initial["codex_runtime_sources"]
    );
    assert_eq!(saved["native_catalog_path"], initial["native_catalog_path"]);
    assert_eq!(
        provider_api_key(
            &saved["providers"][0],
            &server.state.backend.configuration.vault
        ),
        "synthetic-test-key"
    );
    server.shutdown().expect("shutdown");
    let restarted = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &root.join("config.json"),
        "codex",
        root.join("codex/auth.json"),
    )
    .expect("restart");
    assert_eq!(
        parsed_body(&request(&restarted, "/api/config", &[&cookie])),
        actual[4]["payload"]
    );
    restarted.shutdown().expect("shutdown restarted");
}

#[test]
fn narrow_catalog_preference_update_requires_session_and_preserves_new_accounts() {
    let (_directory, server) = test_server();
    let cookie = session_header(&server);
    let stale_page = parsed_body(&request(&server, "/api/config", &[&cookie]));
    assert!(stale_page["accounts"].as_array().unwrap().is_empty());

    let mut newer_config = stale_page.clone();
    newer_config["accounts"] = json!([{
        "id":"added-later", "name":"Added later", "prefix":"later", "enabled":true
    }]);
    let add_account = post(
        &server,
        "/api/config",
        &serde_json::to_vec(&newer_config).expect("new account config"),
        &[&cookie],
    );
    assert!(add_account.starts_with("HTTP/1.1 200"), "{add_account}");

    let preference = br#"{"catalog_show_context":false}"#;
    assert!(
        post(&server, "/api/catalog/context-preference", preference, &[])
            .starts_with("HTTP/1.1 401")
    );
    let origin = format!(
        "Origin: http://attacker.invalid:{}",
        server.local_addr().port()
    );
    assert!(
        post(
            &server,
            "/api/catalog/context-preference",
            preference,
            &[&cookie, &origin]
        )
        .starts_with("HTTP/1.1 403")
    );

    let saved = post(
        &server,
        "/api/catalog/context-preference",
        preference,
        &[&cookie],
    );
    assert!(saved.starts_with("HTTP/1.1 200"), "{saved}");
    assert_eq!(parsed_body(&saved)["catalog_show_context"], false);
    let public = parsed_body(&request(&server, "/api/config", &[&cookie]));
    assert_eq!(public["catalog_show_context"], false);
    assert_eq!(public["accounts"][0]["id"], "added-later");
    let persisted = load_configuration(Some(&server.state.backend.configuration.config_path))
        .expect("persisted config");
    assert_eq!(persisted["catalog_show_context"], false);
    assert_eq!(persisted["accounts"][0]["id"], "added-later");

    let before_invalid = std::fs::read(&server.state.backend.configuration.config_path)
        .expect("config before invalid request");
    let broad_payload = br#"{"catalog_show_context":true,"accounts":[]}"#;
    assert!(
        post(
            &server,
            "/api/catalog/context-preference",
            broad_payload,
            &[&cookie]
        )
        .starts_with("HTTP/1.1 400")
    );
    assert_eq!(
        std::fs::read(&server.state.backend.configuration.config_path)
            .expect("config after invalid request"),
        before_invalid
    );
    server.shutdown().expect("shutdown");
}

#[test]
fn startup_and_save_move_duplicate_visibility_to_native_without_touching_auth() {
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let path = root.join("config.json");
    let native_path = root.join("codex/auth.json");
    let native_bytes =
        br#"{"tokens":{"access_token":"shared-fixture-token","account_id":"native-owner"}}"#;
    std::fs::write(&native_path, native_bytes).expect("native fixture");
    let mut config = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .expect("config")
        .clone();
    let auth_path =
        emp_state::account_auth_path(&config, "duplicate", &path).expect("account path");
    server.state.backend.configuration.vault.write_encrypted_json(&auth_path,&json!({"tokens":{"access_token":"shared-fixture-token","account_id":"different-owner"}})).expect("encrypted auth");
    config["accounts"] = json!([{"id":"duplicate","prefix":"duplicate","auth_file":auth_path,"hidden_models":["native"]}]);
    save_configuration(
        &config,
        Some(&path),
        &server.state.backend.configuration.vault,
    )
    .expect("fixture config");
    server.shutdown().expect("shutdown fixture server");
    let restarted = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        native_path.clone(),
    )
    .expect("restart");
    let cookie = session_header(&restarted);
    let mut public = parsed_body(&request(&restarted, "/api/config", &[&cookie]));
    assert_eq!(public["native_hidden_models"], json!(["native"]));
    assert_eq!(public["accounts"][0]["hidden_models"], json!([]));
    assert_eq!(public["accounts"][0]["duplicate"], true);
    assert_eq!(public["accounts"][0]["duplicate_of"], "当前 Codex 登录");
    let accounts = parsed_body(&request(&restarted, "/api/accounts", &[&cookie]));
    assert_eq!(accounts["accounts"][0]["duplicate"], true);
    let models = parsed_body(&request(&restarted, "/v1/models", &[]));
    assert_eq!(
        models["data"]
            .as_array()
            .expect("models")
            .iter()
            .map(|model| model["id"].clone())
            .collect::<Vec<_>>(),
        vec![json!("demo/old")]
    );
    public["accounts"][0]["hidden_models"] = json!(["native", "missing"]);
    let response = post(
        &restarted,
        "/api/config",
        &serde_json::to_vec(&public).expect("body"),
        &[&cookie],
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(
        parsed_body(&response)["native_hidden_models"],
        json!(["missing", "native"])
    );
    assert_eq!(
        parsed_body(&response)["accounts"][0]["hidden_models"],
        json!([])
    );
    assert_eq!(
        std::fs::read(&native_path).expect("native unchanged"),
        native_bytes
    );
    let saved = std::fs::read(&path).expect("persisted");
    restarted.shutdown().expect("shutdown");
    let second = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        native_path,
    )
    .expect("second restart");
    assert_eq!(std::fs::read(&path).expect("config unchanged"), saved);
    second.shutdown().expect("shutdown second restart");
}

#[cfg(unix)]
#[test]
fn failed_config_commit_restores_provider_secret_and_keeps_memory_snapshot() {
    use std::os::unix::fs::symlink;
    let upstream = CatalogUpstream::start(200);
    let (directory, server) = catalog_server(&upstream);
    let root = canonical_root(&directory);
    let before = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .expect("config")
        .clone();
    let key_path = Path::new(
        before["providers"][0]["api_key_file"]
            .as_str()
            .expect("encrypted key path"),
    );
    let encrypted = std::fs::read(key_path).expect("old encrypted key");
    let cookie = session_header(&server);
    let mut incoming = parsed_body(&request(&server, "/api/config", &[&cookie]));
    incoming["providers"][0]["api_key"] = json!("replacement-fixture-key");
    let protected = root.join("protected.txt");
    std::fs::write(&protected, b"protected fixture").expect("protected file");
    std::fs::rename(root.join("config.json"), root.join("config.original.json"))
        .expect("retain original");
    symlink(&protected, root.join("config.json")).expect("unsafe destination");
    let result = post(
        &server,
        "/api/config",
        &serde_json::to_vec(&incoming).expect("JSON"),
        &[&cookie],
    );
    assert!(result.starts_with("HTTP/1.1 500"), "{result}");
    assert!(!result.contains("replacement-fixture-key"));
    assert_eq!(
        *server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .expect("config"),
        before
    );
    assert_eq!(
        std::fs::read(key_path).expect("restored encrypted key"),
        encrypted
    );
    assert_eq!(
        std::fs::read(&protected).expect("protected"),
        b"protected fixture"
    );
    server.shutdown().expect("shutdown");
}
