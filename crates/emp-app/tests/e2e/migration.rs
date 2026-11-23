use crate::support::{Emp, Upstream, Workspace, import_account};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

fn source(upstream: &Upstream) -> (Workspace, Emp) {
    let workspace = Workspace::new();
    workspace.write_config(json!({"codex_base_url":upstream.base_url(),
        "account_store_path":workspace.root.join("accounts"),
        "providers":[
            {"id":"selected-api", "name":"Selected API", "base_url":upstream.base_url(),
             "protocol":"responses", "auth_mode":"api_key", "api_key":"selected-api-key"},
            {"id":"omitted-api", "name":"Omitted API", "base_url":upstream.base_url(),
             "protocol":"responses", "auth_mode":"api_key", "api_key":"omitted-api-key"}],
        "models":[
            {"id":"selected-api/keep", "provider":"selected-api", "upstream_id":"kept-model", "enabled":true},
            {"id":"selected-api/omit", "provider":"selected-api", "upstream_id":"omitted-model", "enabled":true},
            {"id":"omitted-api/model", "provider":"omitted-api", "upstream_id":"other-model", "enabled":true}]}));
    let emp = workspace.start_without_path_cli();
    for (id, token, owner) in [
        (
            "selected-account",
            "selected-account-token",
            "selected-owner",
        ),
        ("omitted-account", "omitted-account-token", "omitted-owner"),
    ] {
        import_account(&emp, id, token, owner);
    }
    (workspace, emp)
}

fn destination(upstream: &Upstream) -> (Workspace, Emp) {
    let workspace = Workspace::new();
    workspace.write_config(json!({"codex_base_url":upstream.base_url(),
        "account_store_path":workspace.root.join("accounts"),
        "providers":[{"id":"existing-api", "name":"Existing API", "base_url":upstream.base_url(),
            "protocol":"responses", "auth_mode":"api_key", "api_key":"existing-api-key"}],
        "models":[{"id":"existing-api/model", "provider":"existing-api", "upstream_id":"existing-model", "enabled":true}]}));
    let emp = workspace.start_without_path_cli();
    import_account(
        &emp,
        "existing-account",
        "existing-account-token",
        "existing-owner",
    );
    (workspace, emp)
}

fn selections() -> Value {
    json!([
        {"kind":"account", "id":"selected-account", "models":[{"id":"model", "selected":true}]},
        {"kind":"provider", "id":"selected-api", "models":[
            {"id":"selected-api/keep", "selected":true},
            {"id":"selected-api/omit", "selected":false}]}])
}

fn confirmation(emp: &Emp) -> Value {
    let response = emp.post("/api/migration/export/confirm", &json!({}));
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(response.json()["confirmation"].is_string());
    response.json()["confirmation"].clone()
}

fn export(emp: &Emp) -> Value {
    let response = emp.post(
        "/api/migration/export",
        &json!({
        "password":"fixture-migration-password", "confirmation":confirmation(emp),
        "selections":selections()}),
    );
    assert_eq!(response.status, 200, "{}", response.text());
    let payload = response.json();
    assert_eq!(payload["summary"]["accounts"], 1);
    assert_eq!(payload["summary"]["providers"], 1);
    assert_eq!(payload["summary"]["models"], 1);
    payload["bundle"].clone()
}

fn forward(emp: &Emp, upstream: &Upstream, route: &str, token: &str, owner: Option<&str>) {
    let response = emp.responses(&json!({"model":route, "input":"migration fixture"}));
    assert_eq!(response.status, 200, "{route}: {}", response.text());
    let request = upstream.next_forwarded();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(request.headers["authorization"], format!("Bearer {token}"));
    assert_eq!(
        request
            .headers
            .get("chatgpt-account-id")
            .map(String::as_str),
        owner
    );
    let expected_model = match route {
        "selected-api/keep" => "kept-model",
        "existing-api/model" => "existing-model",
        _ => "model",
    };
    assert_eq!(
        request.body.expect("forwarded JSON")["model"],
        expected_model
    );
}

fn assert_selected_import(emp: &Emp, upstream: &Upstream) {
    let response = emp.get("/api/config");
    assert_eq!(response.status, 200, "{}", response.text());
    let config = response.json();
    for (field, expected) in [
        ("providers", vec!["existing-api", "selected-api"]),
        ("accounts", vec!["existing-account", "selected-account"]),
        ("models", vec!["existing-api/model", "selected-api/keep"]),
    ] {
        let mut ids: Vec<_> = config[field]
            .as_array()
            .expect("imported records")
            .iter()
            .map(|row| row["id"].as_str().expect("record id"))
            .collect();
        ids.sort();
        assert_eq!(ids, expected, "{field}: {config}");
    }
    for (route, token, owner) in [
        ("selected-api/keep", "selected-api-key", None),
        (
            "selected-account/model",
            "selected-account-token",
            Some("selected-owner"),
        ),
        ("existing-api/model", "existing-api-key", None),
        (
            "existing-account/model",
            "existing-account-token",
            Some("existing-owner"),
        ),
    ] {
        forward(emp, upstream, route, token, owner);
    }
    upstream.assert_idle();
}

fn upstream() -> Upstream {
    let upstream = Upstream::start();
    upstream.reply_json(&json!({"id":"resp_migration", "object":"response",
        "status":"completed", "model":"model", "output":[]}));
    upstream
}

#[test]
fn selected_file_export_import_preserves_usable_credentials_across_instances() {
    let upstream = upstream();
    let (_source_workspace, sender) = source(&upstream);
    let (workspace, receiver) = destination(&upstream);
    let response = receiver.post(
        "/api/migration/import",
        &json!({
        "password":"fixture-migration-password", "bundle":export(&sender)}),
    );
    assert_eq!(response.status, 200, "{}", response.text());
    assert_selected_import(&receiver, &upstream);
    receiver.stop();
    let restarted = workspace.start_without_path_cli();
    assert_selected_import(&restarted, &upstream);
}

#[test]
fn rejected_file_import_leaves_destination_configuration_and_credentials_intact() {
    let upstream = upstream();
    let (_source_workspace, sender) = source(&upstream);
    let (workspace, receiver) = destination(&upstream);
    let bundle = export(&sender);
    let raw = STANDARD
        .decode(bundle.as_str().expect("bundle base64"))
        .expect("decode fixture bundle");
    let credential_path = workspace
        .root
        .join("accounts/existing-account/auth.json.enc");
    let credential = std::fs::read(&credential_path).expect("destination credential");
    for (case, password, package) in [
        ("wrong password", "incorrect-password", bundle.clone()),
        (
            "truncated package",
            "fixture-migration-password",
            json!(STANDARD.encode(&raw[..raw.len() / 2])),
        ),
    ] {
        // Snapshot per case: the previous case's forwarded requests may persist
        // learned context calibration into the configuration.
        let before = std::fs::read(&workspace.config_path).expect("destination configuration");
        let config = receiver.get("/api/config").json();
        let rejected = receiver.post(
            "/api/migration/import",
            &json!({"password":password, "bundle":package}),
        );
        assert_eq!(rejected.status, 400, "{case}: {}", rejected.text());
        assert_eq!(
            std::fs::read(&workspace.config_path).unwrap(),
            before,
            "{case}"
        );
        assert_eq!(
            std::fs::read(&credential_path).unwrap(),
            credential,
            "{case}"
        );
        let after = receiver.get("/api/config").json();
        for field in ["providers", "accounts", "models"] {
            assert_eq!(after[field], config[field], "{case}: {field}");
        }
        assert!(
            !workspace
                .root
                .join("accounts/selected-account/auth.json.enc")
                .exists()
        );
        forward(
            &receiver,
            &upstream,
            "existing-api/model",
            "existing-api-key",
            None,
        );
        forward(
            &receiver,
            &upstream,
            "existing-account/model",
            "existing-account-token",
            Some("existing-owner"),
        );
    }
    let config = receiver.get("/api/config").json();
    receiver.stop();
    let restarted = workspace.start_without_path_cli();
    let after = restarted.get("/api/config").json();
    for field in ["providers", "accounts", "models"] {
        assert_eq!(after[field], config[field]);
    }
    upstream.assert_idle();
}

#[test]
fn lan_migration_requires_sender_approval_then_imports_selected_services() {
    let upstream = upstream();
    let (_source_workspace, sender) = source(&upstream);
    let (_destination_workspace, receiver) = destination(&upstream);
    let before = receiver.get("/api/config").json();
    let started = sender.post(
        "/api/migration/lan/start",
        &json!({
        "address":"127.0.0.1", "confirmation":confirmation(&sender), "selections":selections()}),
    );
    assert_eq!(started.status, 200, "{}", started.text());
    assert_eq!(started.json()["status"], "waiting");
    let code = started.json()["code"].clone();
    let receive = json!({"code":code, "receiver":"17".repeat(32)});
    let waiting = receiver.post("/api/migration/lan/receive", &receive);
    assert_eq!(waiting.status, 200, "{}", waiting.text());
    assert_eq!(waiting.json()["status"], "waiting");
    assert!(waiting.json()["token"].is_null());
    let status = sender.post("/api/migration/lan/status", &json!({}));
    assert_eq!(status.status, 200, "{}", status.text());
    assert_eq!(status.json()["status"], "confirmation");
    assert_eq!(status.json()["receiver"], "127.0.0.1");
    let approved = sender.post("/api/migration/lan/approve", &json!({"code":code}));
    assert_eq!(approved.status, 200, "{}", approved.text());
    let decoded = receiver.post("/api/migration/lan/receive", &receive);
    assert_eq!(decoded.status, 200, "{}", decoded.text());
    assert!(decoded.json()["token"].is_string());
    let after = receiver.get("/api/config").json();
    for field in ["providers", "accounts", "models"] {
        assert_eq!(
            after[field], before[field],
            "receiving must only stage a preview"
        );
    }
    let imported = receiver.post(
        "/api/migration/import",
        &json!({
        "token":decoded.json()["token"], "selections":selections()}),
    );
    assert_eq!(imported.status, 200, "{}", imported.text());
    assert_selected_import(&receiver, &upstream);
    let status = sender.post("/api/migration/lan/status", &json!({}));
    assert_eq!(status.status, 200, "{}", status.text());
    assert_eq!(status.json()["status"], "sent");
    let replay = receiver.post("/api/migration/lan/receive", &receive);
    assert_eq!(
        replay.status,
        400,
        "LAN delivery must be one-use: {}",
        replay.text()
    );
}
