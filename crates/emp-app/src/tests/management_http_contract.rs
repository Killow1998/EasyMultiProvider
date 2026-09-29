//! Real server contract tests.
use super::*;
#[cfg(unix)]
use std::sync::Mutex;

fn response_json(response: &str) -> Value {
    serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("HTTP response separator")
            .1,
    )
    .expect("HTTP response JSON")
}

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
    let export_body = br#"{"password":"migration-pass","groups":["external"]}"#;
    let unconfirmed = post(
        &source,
        "/api/migration/export",
        export_body,
        &[&session_header(&source)],
    );
    assert!(
        unconfirmed.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{unconfirmed}"
    );
    assert!(unconfirmed.contains("export_confirmation_required"));
    let confirmation = export_confirmation(&source);
    let export = post(
        &source,
        "/api/migration/export",
        &export_request(&confirmation),
        &[&session_header(&source)],
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
        "password":"migration-pass",
        "bundle":STANDARD.encode(bundle)
    }))
    .unwrap();
    let imported = post(
        &target,
        "/api/migration/import",
        &import_body,
        &[&session_header(&target)],
    );
    assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
    let config = request(&target, "/api/config", &[&session_header(&target)]);
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
            "id":"demo","name":"Demo","prefix":"demo","enabled":true,
            "auth_json":{"tokens":{"access_token":"account-secret","account_id":"account-id"}}
        }))
        .unwrap(),
        &[&session_header(&server)],
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
    let removed = delete(&server, "/api/accounts/demo", &[&session_header(&server)]);
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
            &session_header(&server),
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
    // An empty model picker must be rejected; this success scenario needs one
    // visible external model in the fixture.
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
    let before = request(&server, "/api/integration", &[&session_header(&server)]);
    assert!(before.contains("\"state\":\"native\""), "{before}");
    let enabled = post(
        &server,
        "/api/integration/enable",
        br#"{"confirm_reload":true}"#,
        &[&session_header(&server)],
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

/// Answers `model/list` on the Codex control socket with whatever list the test sets.
#[cfg(unix)]
fn fake_codex_backend(
    home: &Path,
    models: Arc<Mutex<Vec<Value>>>,
    reject_model_list: bool,
) -> (
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::os::unix::net::UnixListener;
    let directory = home.join("app-server-control");
    std::fs::create_dir_all(&directory).unwrap();
    let listener = UnixListener::bind(directory.join("app-server-control.sock")).unwrap();
    let refresh_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_refresh_requests = Arc::clone(&refresh_requests);
    let catalog_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_catalog_requests = Arc::clone(&catalog_requests);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let models = Arc::clone(&models);
            let refresh_requests = Arc::clone(&observed_refresh_requests);
            let catalog_requests = Arc::clone(&observed_catalog_requests);
            thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0_u8];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let key = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("sec-websocket-key")
                            .then(|| value.trim().to_owned())
                    })
                    .unwrap();
                let accept = websocket_accept(&key).unwrap();
                write!(stream, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").unwrap();
                let mut websocket = WebSocketConnection::new(&mut stream);
                let mut initialized = false;
                while let Ok(Some(text)) = websocket.receive_text() {
                    let message: Value = serde_json::from_str(&text).unwrap();
                    let reply = match message["method"].as_str() {
                        Some("initialize") => {
                            assert!(
                                !initialized,
                                "control connection initialized more than once"
                            );
                            initialized = true;
                            json!({"result":{}})
                        }
                        Some("config/batchWrite") => {
                            refresh_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            json!({"error":{"code":-32601,"message":"Unexpected config mutation"}})
                        }
                        Some("model/list") if reject_model_list => {
                            catalog_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            json!({"error":{"code":-32000,"message":"fixture model catalog error"}})
                        }
                        Some("model/list") => {
                            catalog_requests.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            json!({"result":{"data":models.lock().unwrap().clone()}})
                        }
                        _ => continue,
                    };
                    if message.get("id").is_some() {
                        let mut reply = reply;
                        reply["id"] = message["id"].clone();
                        let _ = websocket.send_json(&reply);
                    }
                }
            });
        }
    });
    (refresh_requests, catalog_requests)
}

#[cfg(unix)]
#[test]
fn codex_reaching_emp_reports_the_loaded_catalog_without_page_polling() {
    // The Codex control socket lives under this directory, and Unix socket
    // paths are short (104 bytes on macOS), so stay out of the long TMPDIR.
    let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let root = canonical_root(&directory);
    let config = root.join("emp-config.json");
    std::fs::write(&config, serde_json::to_vec(&json!({
        "providers":[{"id":"external","base_url":"https://example.invalid/v1","protocol":"responses"}],
        "models":[{"id":"external/model-a","provider":"external","upstream_id":"model-a","enabled":true}]
    })).unwrap()).unwrap();
    std::fs::write(root.join("config.toml"), b"").unwrap();
    // Codex is running, still on the catalog it loaded before EMP was applied.
    let codex_models = Arc::new(Mutex::new(Vec::new()));
    let (refresh_requests, catalog_requests) =
        fake_codex_backend(&root, Arc::clone(&codex_models), false);
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let session = session_header(&server);
    let enabled = post(
        &server,
        "/api/integration/enable",
        br#"{"confirm_reload":true}"#,
        &[&session],
    );
    assert!(
        enabled.contains("\"state\":\"reload_required\""),
        "{enabled}"
    );
    let mut events = open_quota_events(&server, &session);

    // Codex restarts and loads EMP's catalog, then asks EMP for models.
    let codex_config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    let catalog_path = codex_config
        .lines()
        .find_map(|line| line.strip_prefix("model_catalog_json = "))
        .unwrap()
        .trim_matches('"');
    let catalog: Value = serde_json::from_slice(&std::fs::read(catalog_path).unwrap()).unwrap();
    *codex_models.lock().unwrap() = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|model| model["visibility"].as_str().unwrap_or("list") == "list")
        .map(|model| {
            let slug = model["slug"].as_str().unwrap();
            json!({"id":slug,"displayName":model["display_name"].as_str().unwrap_or(slug),"description":model["description"].as_str().unwrap_or("")})
        })
        .collect();
    let _ = request(&server, "/v1/models", &[]);

    assert_eq!(
        read_sse_frame(&mut events),
        "event: integration-updated\ndata: {}\n"
    );
    let status = request(&server, "/api/integration", &[&session]);
    let status = response_json(&status);
    assert_eq!(status["configuration"]["persisted"], true);
    assert_eq!(status["runtime"]["state"], "catalog_loaded");
    assert_eq!(status["runtime"]["catalog_verified"], true);
    assert_eq!(status["runtime"]["verification_scope"], "emp_model_catalog");
    assert_eq!(status["runtime"]["routing_verified"], false);
    assert_eq!(
        refresh_requests.load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // A config conflict takes precedence over the previously observed catalog.
    let applied_config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    let mut changed_url = false;
    let conflicting_config = applied_config
        .lines()
        .map(|line| {
            if line.starts_with("openai_base_url = ") {
                changed_url = true;
                "openai_base_url = \"https://other.example/v1\""
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(changed_url);
    std::fs::write(root.join("config.toml"), &conflicting_config).unwrap();
    let conflicted = response_json(&request(&server, "/api/integration", &[&session]));
    assert_eq!(conflicted["configuration"]["state"], "conflict");
    assert_eq!(conflicted["runtime"]["state"], "not_checked");
    assert!(conflicted["runtime"]["target"].is_null());
    assert_eq!(conflicted["runtime"]["catalog_verified"], false);
    assert_eq!(conflicted["runtime"]["verification_scope"], "none");
    assert!(conflicted["runtime"]["target_matches_saved_configuration"].is_null());
    assert_eq!(conflicted["runtime"]["routing_verified"], false);
    assert_eq!(conflicted["runtime"]["restoration_verified"], false);
    assert!(
        conflicted["next_action"]
            .as_str()
            .unwrap()
            .contains("configuration conflict")
    );
    let catalog_requests_before_conflict_check =
        catalog_requests.load(std::sync::atomic::Ordering::Relaxed);
    let rejected_check = post(&server, "/api/integration/reload", b"{}", &[&session]);
    assert!(
        rejected_check.starts_with("HTTP/1.1 409 Error\r\n"),
        "{rejected_check}"
    );
    let rejected_status = response_json(&rejected_check);
    assert_eq!(rejected_status["configuration"]["state"], "conflict");
    assert!(
        rejected_status["error"]["message"]
            .as_str()
            .unwrap()
            .contains("configuration is unresolved")
    );
    let support = response_json(&request(&server, "/api/support-report", &[&session]));
    assert_eq!(support["codex"]["state"], "not_checked");
    assert_eq!(support["codex"]["target"], "unknown");
    assert_eq!(
        std::fs::read_to_string(root.join("config.toml")).unwrap(),
        conflicting_config
    );
    assert_eq!(
        catalog_requests.load(std::sync::atomic::Ordering::Relaxed),
        catalog_requests_before_conflict_check
    );
    std::fs::write(root.join("config.toml"), applied_config).unwrap();
    let status = response_json(&request(&server, "/api/integration", &[&session]));
    assert_eq!(status["configuration"]["state"], "emp_applied");
    assert_eq!(status["runtime"]["state"], "catalog_loaded");

    // An external config change makes the previous live catalog observation stale.
    let externally_restored = server.state.backend.integration.manager.restore().unwrap();
    assert!(externally_restored.ok());
    let status = response_json(&request(&server, "/api/integration", &[&session]));
    assert_eq!(status["configuration"]["state"], "native");
    assert_eq!(status["runtime"]["state"], "not_checked");
    assert_eq!(status["runtime"]["target"], "native");
    assert_eq!(status["runtime"]["confidence"], "stale");
    assert_eq!(status["runtime"]["last_known"]["state"], "catalog_loaded");
    assert_eq!(status["runtime"]["last_known"]["target"], "emp");
    let support = response_json(&request(&server, "/api/support-report", &[&session]));
    assert_eq!(support["codex"]["state"], "not_checked");
    assert_eq!(support["codex"]["target"], "native");
    assert_eq!(support["codex"]["verification_scope"], "none");

    // Probe the already loaded catalog before the explicit restore closes
    // management admission and asks this EMP process to stop.
    *codex_models.lock().unwrap() = vec![json!({"id":"gpt-5.5"})];
    let native_config_before_check = std::fs::read(root.join("config.toml")).unwrap();
    let catalog_requests_before_check = catalog_requests.load(std::sync::atomic::Ordering::Relaxed);
    let checked = post(&server, "/api/integration/reload", b"{}", &[&session]);
    assert!(checked.starts_with("HTTP/1.1 200 OK\r\n"), "{checked}");
    let status = response_json(&checked);
    assert_eq!(status["runtime"]["state"], "emp_catalog_absent");
    assert_eq!(status["runtime"]["target"], "native");
    assert_eq!(status["runtime"]["catalog_verified"], false);
    assert_eq!(status["runtime"]["emp_models_absent"], true);
    assert_eq!(status["runtime"]["restoration_verified"], false);
    assert_eq!(status["runtime"]["routing_verified"], false);
    assert_eq!(
        std::fs::read(root.join("config.toml")).unwrap(),
        native_config_before_check
    );
    assert_eq!(
        catalog_requests.load(std::sync::atomic::Ordering::Relaxed),
        catalog_requests_before_check + 1
    );
    assert_eq!(
        refresh_requests.load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // A runtime observation record is best-effort metadata. Its write failure
    // must not turn a committed native restore into an error or stop before
    // the UI receives the restore result.
    let runtime_path = server
        .state
        .backend
        .integration
        .manager
        .lease_path()
        .with_file_name("runtime.json");
    let previous_runtime = std::fs::read(&runtime_path).unwrap();
    std::fs::remove_file(&runtime_path).unwrap();
    std::fs::create_dir(&runtime_path).unwrap();

    let restored = post(
        &server,
        "/api/integration/restore",
        br#"{"confirm_reload":true}"#,
        &[&session],
    );
    assert!(restored.starts_with("HTTP/1.1 200 OK\r\n"), "{restored}");
    let restored_result = response_json(&restored);
    assert_eq!(restored_result["configuration"]["state"], "native");
    assert_eq!(restored_result["runtime"]["target"], "native");
    // The client returns after reading Content-Length, before the handler's
    // half-close and shutdown store necessarily run. Wait for the observable
    // post-response transition with a deadline.
    let shutdown_deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !server
        .state
        .shutdown
        .load(std::sync::atomic::Ordering::Acquire)
        && std::time::Instant::now() < shutdown_deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        server
            .state
            .shutdown
            .load(std::sync::atomic::Ordering::Acquire)
    );
    let restored_config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert_eq!(restored_config.as_bytes(), native_config_before_check);
    assert!(!restored_config.contains("model_catalog_json"));
    server.shutdown().unwrap();
    std::fs::remove_dir(&runtime_path).unwrap();
    std::fs::write(&runtime_path, previous_runtime).unwrap();

    // A new EMP process reports the persisted target as unverified and keeps
    // the prior catalog observation only as last-known data.
    let restarted = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let status = response_json(&request(
        &restarted,
        "/api/integration",
        &[&session_header(&restarted)],
    ));
    assert_eq!(status["configuration"]["state"], "native");
    assert_eq!(status["runtime"]["state"], "not_checked");
    assert_eq!(status["runtime"]["target"], "native");
    assert_eq!(status["runtime"]["confidence"], "stale");
    assert_eq!(
        status["runtime"]["last_known"]["state"],
        "emp_catalog_absent"
    );
    assert_eq!(status["runtime"]["last_known"]["catalog_verified"], false);
    assert_eq!(status["runtime"]["last_known"]["emp_models_absent"], true);
    assert_eq!(status["runtime"]["last_known"]["routing_verified"], false);
    assert_eq!(
        status["runtime"]["last_known"]["restoration_verified"],
        false
    );
    restarted.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn integration_reports_model_list_errors_without_undoing_saved_config() {
    let directory = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let root = canonical_root(&directory);
    let config = root.join("emp-config.json");
    std::fs::write(&config, serde_json::to_vec(&json!({
        "providers":[{"id":"external","base_url":"https://example.invalid/v1","protocol":"responses"}],
        "models":[{"id":"external/model-a","provider":"external","upstream_id":"model-a","enabled":true}]
    })).unwrap()).unwrap();
    std::fs::write(root.join("config.toml"), b"").unwrap();
    let codex_models = Arc::new(Mutex::new(Vec::new()));
    let (refresh_requests, _) = fake_codex_backend(&root, codex_models, true);
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-codex",
        root.join("auth.json"),
    )
    .unwrap();
    let session = session_header(&server);

    let enabled = post(
        &server,
        "/api/integration/enable",
        br#"{"confirm_reload":true}"#,
        &[&session],
    );
    assert!(enabled.starts_with("HTTP/1.1 200 OK\r\n"), "{enabled}");
    let status = response_json(&enabled);
    assert_eq!(status["configuration"]["state"], "emp_applied");
    assert_eq!(status["runtime"]["state"], "verification_failed");
    assert!(
        status["runtime"]["detail"]
            .as_str()
            .unwrap()
            .contains("rejected the model catalog query")
    );
    assert_eq!(
        refresh_requests.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(
        std::fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("openai_base_url")
    );

    let checked = post(&server, "/api/integration/reload", b"{}", &[&session]);
    assert!(checked.starts_with("HTTP/1.1 409 Error\r\n"), "{checked}");
    let status = response_json(&checked);
    assert_eq!(status["configuration"]["state"], "emp_applied");
    assert_eq!(status["runtime"]["state"], "verification_failed");
    assert_eq!(status["runtime"]["routing_verified"], false);
    server.shutdown().unwrap();
}

fn export_confirmation(server: &ServerHandle) -> String {
    let response = post(
        server,
        "/api/migration/export/confirm",
        b"{}",
        &[&session_header(server)],
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").expect("separator").1)
        .expect("confirmation JSON");
    assert_eq!(body["expires_in"], 60);
    body["confirmation"]
        .as_str()
        .expect("confirmation token")
        .to_owned()
}

fn export_request(confirmation: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "password":"migration-pass",
        "groups":["external"],
        "confirmation":confirmation,
    }))
    .expect("export request JSON")
}

#[test]
fn migration_export_requires_twelve_utf8_password_bytes() {
    let (_directory, server) = test_server();
    assert_eq!("密码好ab".len(), 11);
    assert_eq!("四海升平".len(), 12);
    let confirmation = export_confirmation(&server);
    let short_password = serde_json::to_vec(&json!({
        "password":"密码好ab",
        "groups":["external"],
        "confirmation":confirmation,
    }))
    .expect("short export request JSON");
    let rejected = post(
        &server,
        "/api/migration/export",
        &short_password,
        &[&session_header(&server)],
    );
    assert!(
        rejected.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{rejected}"
    );
    assert!(rejected.contains("migration_password_too_short"));
    let spent = post(
        &server,
        "/api/migration/export",
        &export_request(&confirmation),
        &[&session_header(&server)],
    );
    assert!(spent.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{spent}");

    let confirmation = export_confirmation(&server);
    let exact_password = serde_json::to_vec(&json!({
        "password":"四海升平",
        "groups":["external"],
        "confirmation":confirmation,
    }))
    .expect("exact-byte export request JSON");
    let accepted = post(
        &server,
        "/api/migration/export",
        &exact_password,
        &[&session_header(&server)],
    );
    assert!(accepted.starts_with("HTTP/1.1 200 OK\r\n"), "{accepted}");
    server.shutdown().expect("shutdown");
}

#[test]
fn migration_export_confirmation_is_single_use_and_session_bound() {
    let (_directory, server) = test_server();
    let unauthenticated = post(&server, "/api/migration/export/confirm", b"{}", &[]);
    assert!(unauthenticated.starts_with("HTTP/1.1 401 Unauthorized\r\n"));

    let confirmation = export_confirmation(&server);
    let wrong = post(
        &server,
        "/api/migration/export",
        &export_request("wrong"),
        &[&session_header(&server)],
    );
    assert!(wrong.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{wrong}");
    // A failed attempt spends the pending confirmation.
    let spent = post(
        &server,
        "/api/migration/export",
        &export_request(&confirmation),
        &[&session_header(&server)],
    );
    assert!(spent.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{spent}");

    let confirmation = export_confirmation(&server);
    let exported = post(
        &server,
        "/api/migration/export",
        &export_request(&confirmation),
        &[&session_header(&server)],
    );
    assert!(exported.starts_with("HTTP/1.1 200 OK\r\n"), "{exported}");
    let replayed = post(
        &server,
        "/api/migration/export",
        &export_request(&confirmation),
        &[&session_header(&server)],
    );
    assert!(
        replayed.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{replayed}"
    );

    let now = crate::util::system_now();
    let wrong_operation = server
        .state
        .sessions
        .issue_export_confirmation("/api/migration/import", now)
        .expect("issue import-scoped confirmation");
    assert!(!server.state.sessions.consume_export_confirmation(
        &wrong_operation,
        "/api/migration/export",
        now,
    ));

    let confirmation = server
        .state
        .sessions
        .issue_export_confirmation("/api/migration/export", now)
        .expect("issue confirmation");
    server
        .state
        .sessions
        .rotate(now + 1.0)
        .expect("rotate session");
    assert!(!server.state.sessions.consume_export_confirmation(
        &confirmation,
        "/api/migration/export",
        now + 2.0,
    ));

    let expired = server
        .state
        .sessions
        .issue_export_confirmation("/api/migration/export", now + 3.0)
        .expect("issue expiring confirmation");
    assert!(!server.state.sessions.consume_export_confirmation(
        &expired,
        "/api/migration/export",
        now + 64.0,
    ));
    server.shutdown().expect("shutdown");
}
