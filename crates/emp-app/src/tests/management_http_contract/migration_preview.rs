//! Authenticated selected export, read-only preview and transactional commit.
use super::*;

fn json_body(response: &str) -> Value {
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[test]
fn duplicate_preferences_are_resolved_before_atomic_import_and_native_identity_changes_reject_stale_choices()
 {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let path = root.join("config.json");
    let native = root.join("auth.json");
    let native_bytes =
        br#"{"tokens":{"account_id":"fixture-owner","access_token":"private-native-fixture"}}"#;
    std::fs::write(&native, native_bytes).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(
            &json!({"auto_enable_on_start":false,"native_model_context_windows":{"demo":400000}}),
        )
        .unwrap(),
    )
    .unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        native.clone(),
    )
    .unwrap();
    let session = session_header(&server);
    let before = std::fs::read(&path).unwrap();
    let metadata = json!({"id":"incoming","prefix":"incoming","name":"Incoming","hidden_models":["demo"],"model_context_windows":{"demo":467368}});
    let payload = json!({"schema":"easy-multi-provider-migration","version":1,"config":{"accounts":[metadata],"providers":[],"models":[]},"accounts":[{"metadata":metadata,"auth":{"tokens":{"account_id":"fixture-owner","access_token":"private-incoming-fixture"}}}],"provider_keys":{}});
    let bundle = emp_state::migration::encode_migration(
        "fixture-password",
        &[7; 16],
        &serde_json::to_vec(&payload).unwrap(),
    )
    .unwrap();
    let decoded = json_body(&post(
        &server,
        "/api/migration/decode",
        &serde_json::to_vec(
            &json!({"password":"fixture-password","bundle":STANDARD.encode(bundle)}),
        )
        .unwrap(),
        &[&session],
    ));
    let mut body = json!({"token":decoded["token"],"selections":[{"kind":"account","id":"incoming","models":[{"id":"demo","selected":false,"context_window":467368}]}]});
    let pending = json_body(&post(
        &server,
        "/api/migration/preview",
        &serde_json::to_vec(&body).unwrap(),
        &[&session],
    ));
    assert_eq!(pending["preference_conflicts"][0]["target_id"], "@native");
    assert_eq!(
        pending["preference_conflicts"][0]["fields"]
            .as_object()
            .unwrap()
            .len(),
        2
    );
    assert!(!pending.to_string().contains("private-"));
    assert!(
        post(
            &server,
            "/api/migration/import",
            &serde_json::to_vec(&body).unwrap(),
            &[&session]
        )
        .starts_with("HTTP/1.1 400")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    body["preference_choices"] = json!({"incoming":{"visibility":"incoming","context":"current"}});
    let ready = json_body(&post(
        &server,
        "/api/migration/preview",
        &serde_json::to_vec(&body).unwrap(),
        &[&session],
    ));
    assert_eq!(ready["config"]["native_hidden_models"], json!(["demo"]));
    assert_eq!(
        ready["config"]["native_model_context_windows"],
        json!({"demo":400000})
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::write(
        &native,
        br#"{"tokens":{"account_id":"different-owner","access_token":"other-token"}}"#,
    )
    .unwrap();
    assert!(
        post(
            &server,
            "/api/migration/import",
            &serde_json::to_vec(&body).unwrap(),
            &[&session]
        )
        .starts_with("HTTP/1.1 400")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::write(&native, native_bytes).unwrap();
    json_body(&post(
        &server,
        "/api/migration/import",
        &serde_json::to_vec(&body).unwrap(),
        &[&session],
    ));
    let stored = server.state.backend.configuration.snapshot().unwrap();
    assert_eq!(stored["native_hidden_models"], json!(["demo"]));
    assert_eq!(
        stored["accounts"][0]["hidden_models"],
        stored["native_hidden_models"]
    );
    assert_eq!(
        stored["accounts"][0]["model_context_windows"],
        stored["native_model_context_windows"]
    );
    assert_eq!(std::fs::read(native).unwrap(), native_bytes);
    server.shutdown().unwrap();
}

#[test]
fn auth_only_import_inherits_native_preferences_and_reimport_keeps_one_reference() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let path = root.join("config.json");
    let native = root.join("auth.json");
    let native_bytes =
        br#"{"tokens":{"account_id":"fixture-owner","access_token":"native-fixture"}}"#;
    std::fs::write(&native, native_bytes).unwrap();
    std::fs::write(&path,serde_json::to_vec(&json!({"auto_enable_on_start":false,"native_hidden_models":["demo"],"native_model_context_windows":{"demo":400000}})).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        native.clone(),
    )
    .unwrap();
    let session = session_header(&server);
    for token in ["pasted-fixture", "file-fixture"] {
        json_body(&post(&server,"/api/accounts/import",&serde_json::to_vec(&json!({"name":"Imported","auth_json":{"tokens":{"account_id":"fixture-owner","access_token":token}}})).unwrap(),&[&session]));
        let stored = server.state.backend.configuration.snapshot().unwrap();
        assert_eq!(stored["accounts"].as_array().unwrap().len(), 1);
        assert_eq!(
            stored["accounts"][0]["hidden_models"],
            stored["native_hidden_models"]
        );
        assert_eq!(
            stored["accounts"][0]["model_context_windows"],
            stored["native_model_context_windows"]
        );
    }
    assert_eq!(std::fs::read(native).unwrap(), native_bytes);
    server.shutdown().unwrap();
}

#[test]
fn selected_migration_previews_redact_credentials_and_commit_only_selected_names() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let path = root.join("config.json");
    std::fs::write(&path,serde_json::to_vec(&json!({"auto_enable_on_start":false,"providers":[
        {"id":"one","name":"One","base_url":"https://one.example.invalid/v1","protocol":"responses","auth_mode":"api_key","api_key":"private-fixture-one"},
        {"id":"two","name":"Two","base_url":"https://two.example.invalid/v1","protocol":"responses","auth_mode":"api_key","api_key":"private-fixture-two"}],
        "models":[{"id":"one/first","provider":"one","upstream_id":"first","enabled":true},{"id":"one/second","provider":"one","upstream_id":"second","enabled":false},{"id":"two/keep","provider":"two","upstream_id":"keep","enabled":true}]})).unwrap()).unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let session = session_header(&server);
    let before = std::fs::read(&path).unwrap();
    let choices = json!([{"kind":"provider","id":"one","name":"One","models":[{"id":"one/first","selected":true},{"id":"one/second","selected":false}]}]);
    let exported=json_body(&post(&server,"/api/migration/export",&serde_json::to_vec(&json!({"password":"fixture-password","confirmation":export_confirmation(&server),"selections":choices})).unwrap(),&[&session]));
    let plain = emp_state::migration::decode_migration(
        "fixture-password",
        &STANDARD
            .decode(exported["bundle"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let payload: Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(payload["config"]["providers"].as_array().unwrap().len(), 1);
    assert_eq!(payload["config"]["models"].as_array().unwrap().len(), 1);
    assert_eq!(payload["provider_keys"]["one"], "private-fixture-one");
    assert!(payload["provider_keys"].get("two").is_none());
    let decoded = json_body(&post(
        &server,
        "/api/migration/decode",
        &serde_json::to_vec(&json!({"password":"fixture-password","bundle":exported["bundle"]}))
            .unwrap(),
        &[&session],
    ));
    assert!(!decoded.to_string().contains("private-fixture"));
    let request = json!({"token":decoded["token"],"selections":[{"kind":"provider","id":"one","name":"Renamed","models":[{"id":"one/first","selected":true}]}]});
    let preview = json_body(&post(
        &server,
        "/api/migration/preview",
        &serde_json::to_vec(&request).unwrap(),
        &[&session],
    ));
    assert!(!preview.to_string().contains("private-fixture"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(
        preview["config"]["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "one" && p["name"] == "Renamed")
    );
    assert!(
        post(
            &server,
            "/api/migration/preview",
            &serde_json::to_vec(&request).unwrap(),
            &[]
        )
        .starts_with("HTTP/1.1 401")
    );
    json_body(&post(
        &server,
        "/api/migration/import",
        &serde_json::to_vec(&request).unwrap(),
        &[&session],
    ));
    let stored = server.state.backend.configuration.snapshot().unwrap();
    assert!(
        stored["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "one" && p["name"] == "Renamed")
    );
    let other = stored["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "two")
        .unwrap();
    assert_eq!(
        provider_api_key(other, &server.state.backend.configuration.vault),
        "private-fixture-two"
    );
    assert_eq!(
        stored["models"].as_array().unwrap().len(),
        3,
        "Unselected existing models must remain"
    );
    assert!(
        post(
            &server,
            "/api/migration/import",
            &serde_json::to_vec(&request).unwrap(),
            &[&session]
        )
        .starts_with("HTTP/1.1 400"),
        "A completed preview token cannot be replayed"
    );
    server.shutdown().unwrap();
}

#[test]
fn demo_migration_cannot_commit_and_preview_cache_eviction_requires_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let root = canonical_root(&directory);
    let path = root.join("config.json");
    std::fs::write(&path, b"{\"auto_enable_on_start\":false}").unwrap();
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let session = session_header(&server);
    let first = json_body(&post(&server, "/api/migration/demo", b"{}", &[&session]));
    let choices = json!({"token":first["token"],"selections":[{"kind":"provider","id":"sample-api","models":[{"id":"sample-api/sample-model","selected":true}]}]});
    assert!(
        post(
            &server,
            "/api/migration/import",
            &serde_json::to_vec(&choices).unwrap(),
            &[&session]
        )
        .contains("Demo packages cannot be imported")
    );
    for _ in 0..4 {
        json_body(&post(&server, "/api/migration/demo", b"{}", &[&session]));
    }
    assert!(
        post(
            &server,
            "/api/migration/preview",
            &serde_json::to_vec(&choices).unwrap(),
            &[&session]
        )
        .contains("Reopen the migration package")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    server.shutdown().unwrap();
}
