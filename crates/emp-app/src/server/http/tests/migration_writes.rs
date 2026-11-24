use super::{team_writes, *};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
use emp_test_support::{Deadline, test_timeout};
use std::{fs, path::Path};

const PASSWORD: &str = "fixture-migration-password";
const KEY: &str = "fixture-migration-credential";
const OPERATIONS: [&str; 11] = [
    "export/confirm",
    "export",
    "decode",
    "demo",
    "preview",
    "import",
    "lan/start",
    "lan/status",
    "lan/approve",
    "lan/cancel",
    "lan/receive",
];

fn with_fixture(make: impl FnOnce() -> Fixture, test: impl AsyncFnOnce(&Fixture)) {
    make().run_wire(async |fixture, _| test(fixture).await);
}

fn provider(id: &str) -> Value {
    json!({"id":id,"name":id,"base_url":format!("https://{id}.example.invalid/v1"),"protocol":"responses","auth_mode":"api_key"})
}

fn model(id: &str) -> Value {
    json!({"id":format!("{id}/model"),"provider":id,"upstream_id":"model","enabled":true})
}

fn source() -> Fixture {
    let directory = emp_test_support::fixture_dir().unwrap();
    let mut service = provider("source");
    service["api_key"] = KEY.into();
    fs::write(
        directory.path().join("config.json"),
        json!({
            "providers":[service], "models":[model("source")],
        })
        .to_string(),
    )
    .unwrap();
    Fixture::with_directory(directory)
}

fn selection(id: &str) -> Value {
    json!([{"kind":"provider","id":id}])
}

fn bundle(id: &str) -> Vec<u8> {
    let payload = json!({
        "schema":"easy-multi-provider-migration", "version":1,
        "config":{"accounts":[],"providers":[provider(id)],"models":[model(id)]},
        "accounts":[], "provider_keys":{id:KEY},
    });
    emp_state::migration::encode_migration(PASSWORD, &[1; 16], payload.to_string().as_bytes())
        .unwrap()
}

fn split_wire(reply: &[u8]) -> (&str, &[u8]) {
    let separator = reply
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = std::str::from_utf8(&reply[..separator + 2]).unwrap();
    let body = &reply[separator + 4..];
    let length = head
        .lines()
        .filter_map(|line| line.strip_prefix("Content-Length: "))
        .collect::<Vec<_>>();
    assert_eq!(length.len(), 1);
    assert_eq!(length[0].parse::<usize>().unwrap(), body.len());
    (head, body)
}

fn validate_bundle(bundle: &[u8]) {
    let envelope = bundle
        .strip_prefix(emp_state::migration::MIGRATION_MAGIC)
        .expect("migration bundle magic");
    assert!(envelope.ends_with(b"\n"));
    let envelope: Value = serde_json::from_slice(envelope).expect("migration envelope");
    let plaintext = envelope.to_string();
    assert!(
        !plaintext.contains(KEY) && !plaintext.contains(PASSWORD),
        "migration envelope contains fixture credentials"
    );
    assert_eq!(envelope["schema"], "easy-multi-provider-migration");
    assert_eq!(envelope["version"], 2);
    let salt = URL_SAFE.decode(envelope["salt"].as_str().unwrap()).unwrap();
    assert_eq!(salt.len(), 16);
    let payload = URL_SAFE
        .decode(envelope["payload"].as_str().unwrap())
        .unwrap();
    assert!(payload.len() > 100);
}

fn token_field(value: &Value, field: &str) -> String {
    let token = value[field].as_str().expect("migration token");
    assert_eq!(token.len(), 48);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    token.to_owned()
}

fn validate_code(code: &str) {
    assert_eq!(code.len(), 49);
    let bytes = URL_SAFE_NO_PAD
        .decode(code.strip_prefix('E').unwrap())
        .unwrap();
    assert_eq!(bytes.len(), 36);
    assert_eq!(&bytes[..5], &[1, 127, 0, 0, 1]);
    assert_ne!(&bytes[5..7], &[0, 0]);
}

fn check_migration_reply(operation: &str, reply: &[u8]) {
    let (head, body) = split_wire(reply);
    if head.starts_with("HTTP/1.1 200 ")
        && head.contains("Content-Type: application/octet-stream\r\n")
    {
        validate_bundle(body);
        return;
    }
    let value: Value = serde_json::from_slice(body).expect("migration response JSON");
    let text = std::str::from_utf8(body).unwrap();
    assert!(
        !text.contains(KEY) && !text.contains(PASSWORD),
        "migration {operation} response contains fixture credentials"
    );
    if let Some(confirmation) = value["confirmation"].as_str() {
        assert_eq!(confirmation.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(confirmation).unwrap().len(), 32);
    }
    if value.get("token").is_some() {
        token_field(&value, "token");
    }
    if operation == "preview"
        && let Some(providers) = value["config"]["providers"].as_array()
    {
        for provider in providers
            .iter()
            .filter(|row| row.get("_shadow_migration_token").is_some())
        {
            token_field(provider, "_shadow_migration_token");
        }
    }
    if let Some(code) = value["code"].as_str() {
        validate_code(code);
        let remaining = value["remaining_seconds"].as_u64().unwrap();
        assert!((1..=300).contains(&remaining));
    }
    if let Some(encoded) = value["bundle"].as_str() {
        validate_bundle(&STANDARD.decode(encoded).unwrap());
    }
}

fn wire(fixture: &Fixture, operation: &str, authenticated: bool, body: &[u8]) -> Vec<u8> {
    let session = fixture.session();
    let headers = [session.as_str()];
    json_request(
        fixture.port(),
        &format!("/api/migration/{operation}"),
        if authenticated { &headers } else { &[] },
        body,
    )
}

async fn exchange(
    fixture: &Fixture,
    operation: &str,
    authenticated: bool,
    body: &[u8],
    status: u16,
) -> Vec<u8> {
    let wire = wire(fixture, operation, authenticated, body);
    let reply = assert_wire(fixture.server.local_addr(), &wire, false, status).await;
    check_migration_reply(operation, &reply);
    reply
}

async fn post(fixture: &Fixture, operation: &str, body: Value, status: u16) -> Value {
    json_body(
        &exchange(
            fixture,
            operation,
            true,
            body.to_string().as_bytes(),
            status,
        )
        .await,
    )
}

async fn confirmation(fixture: &Fixture) -> String {
    let value = post(fixture, "export/confirm", json!({}), 200).await;
    assert_eq!(value["expires_in"], 60);
    value["confirmation"].as_str().unwrap().to_owned()
}

fn verify_payload(bundle: &[u8]) {
    let decrypted = emp_state::migration::decode_migration(PASSWORD, bundle).unwrap();
    let payload: Value = serde_json::from_slice(&decrypted).unwrap();
    assert!(payload["provider_keys"]["source"] == KEY);
    assert_eq!(payload["config"]["providers"][0]["id"], "source");
    assert_eq!(payload["config"]["models"][0]["id"], "source/model");
}

#[test]
fn quit_releases_service_lock_and_flushes_credentials_with_a_stalled_export() {
    const EXPORT_PAYLOAD_BYTES: usize = 12 * 1024 * 1024;

    let directory = emp_test_support::fixture_dir().unwrap();
    let root = emp_state::config::resolve_user_path(directory.path());
    let auth_path = root.join("state/accounts/kept/auth.json.enc");
    let config_path = root.join("config.json");
    let vault = emp_state::VaultStore::from_sources(None, &root.join("state/master.key")).unwrap();
    vault
        .write_encrypted_json(&auth_path, &json!({"tokens":{"access_token":"stale"}}))
        .unwrap();
    fs::write(
        &config_path,
        json!({
            "providers": [provider("source")],
            "models": [{
                "id": "source/model",
                "upstream_id": "model",
                "description": "x".repeat(EXPORT_PAYLOAD_BYTES),
                "provider": "source",
            }],
            "accounts": [{"id":"kept","name":"kept","prefix":"kept","auth_file":auth_path,"credential_source":"emp_login"}],
        })
        .to_string(),
    )
    .unwrap();
    let fixture = Fixture::with_directory(directory);
    let state = Arc::clone(&fixture.server.state);
    let clock = crate::server::tests::TestDrainClock::install(&state.lifecycle);
    state
        .backend
        .accounts
        .pending_rotations
        .lock()
        .unwrap()
        .insert(
            auth_path.to_str().unwrap().to_owned(),
            json!({"tokens":{"access_token":"rotated"}}),
        );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let slow = runtime.block_on(async {
        let address = fixture.server.local_addr();
        let body = json!({
            "confirmation":confirmation(&fixture).await, "password":PASSWORD, "groups":["external"],
        });
        let slow = crate::server::raw_tcp::RawClient::request(
            address,
            &wire(&fixture, "export", true, body.to_string().as_bytes()),
        )
        .await;
        assert!(slow.head.starts_with("HTTP/1.1 200 "), "{}", slow.head);
        let length = slow
            .head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(length >= EXPORT_PAYLOAD_BYTES, "export size: {length}");
        let reply = assert_wire(
            address,
            &json_request(fixture.port(), "/api/quit", &[&fixture.session()], b"{}"),
            false,
            200,
        )
        .await;
        assert_eq!(json_body(&reply), json!({"status":"stopping"}));
        assert_eq!(
            timeout(
                crate::server::raw_tcp::wire_timeout(),
                state.lifecycle.signal().wait()
            )
            .await
            .unwrap(),
            crate::server::ShutdownReason::ResponseWritten,
        );
        assert!(
            state.lifecycle.admission.active_requests() > 0,
            "the stalled response still owns a connection slot"
        );
        slow
    });
    let result = shutdown_with_stalled_response(fixture.server, slow, &clock);
    let lock = emp_state::IntegrationFileLock::acquire(
        &root.join("state/service.lock"),
        Duration::ZERO,
        Duration::ZERO,
    );
    assert!(lock.is_ok(), "shutdown released service ownership");
    assert_eq!(
        vault.read_encrypted_json(&auth_path).unwrap()["tokens"]["access_token"],
        "rotated"
    );
    let journal = crate::tests::live_journal(&root);
    let receipt = journal
        .iter()
        .find(|entry| {
            entry["event"] == "http_request_completed"
                && entry["fields"]["path"] == "/api/migration/export"
        })
        .expect("export completed");
    assert_eq!(
        (
            result.is_ok(),
            &receipt["fields"]["status"],
            &receipt["fields"]["result"]
        ),
        (true, &json!(200), &json!("write_failed")),
        "{result:?}, {receipt}"
    );
}

#[test]
fn migration_exports_match_wire_and_consume_confirmation_once() {
    with_fixture(source, async |fixture| {
        let body = json!({
            "confirmation":confirmation(fixture).await, "password":PASSWORD, "groups":["external"],
        });
        let reply = exchange(fixture, "export", true, body.to_string().as_bytes(), 200).await;
        let (head, payload) = split_wire(&reply);
        assert!(head.contains("Content-Type: application/octet-stream\r\n"));
        assert!(head.contains("Content-Disposition: attachment; filename=\"EMP.emp\"\r\n"));
        assert!(head.contains("X-EMP-Export-Summary: "));
        verify_payload(payload);
        let result = post(fixture, "export", body, 403).await;
        assert_eq!(result["error"]["code"], "export_confirmation_required");

        let selection = json!({
            "confirmation":confirmation(fixture).await, "password":PASSWORD, "selections":selection("source"),
            "include_context":false,"include_rename":false,
        });
        let value = post(fixture, "export", selection, 200).await;
        verify_payload(&STANDARD.decode(value["bundle"].as_str().unwrap()).unwrap());
        assert_eq!(value["summary"]["providers"], 1);

        let expired = fixture
            .server
            .state
            .sessions
            .issue_export_confirmation(
                crate::http::auth::EXPORT_CONFIRMATION_OPERATION,
                system_now() - 61.0,
            )
            .unwrap();
        let value = post(
            fixture,
            "export",
            json!({"confirmation":expired,"password":PASSWORD}),
            403,
        )
        .await;
        assert_eq!(value["error"]["code"], "export_confirmation_required");
    });
}

fn saved_config(fixture: &Fixture) -> Value {
    let root = fixture.directory.path().canonicalize().unwrap();
    serde_json::from_slice(&fs::read(root.join("config.json")).unwrap()).unwrap()
}

fn assert_imported(fixture: &Fixture, ids: &[&str]) {
    let config = saved_config(fixture);
    let state = &fixture.server.state;
    let catalog = fs::read(crate::services::catalog::generated_catalog_path(state)).unwrap();
    let models: Value = serde_json::from_slice(&catalog).unwrap();
    let providers = config["providers"].as_array().unwrap();
    assert_eq!(providers.len(), ids.len());
    let keys = ids
        .iter()
        .map(|id| {
            let provider = providers
                .iter()
                .find(|provider| provider["id"] == *id)
                .unwrap();
            let key_path = Path::new(provider["api_key_file"].as_str().unwrap());
            assert!(
                fs::read(key_path)
                    .unwrap()
                    .starts_with(emp_state::vault::VAULT_MAGIC)
            );
            assert!(
                models["models"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|model| model["slug"] == format!("{id}/model"))
            );
            emp_state::config::provider_api_key(provider, &state.backend.configuration.vault)
        })
        .collect::<Vec<_>>();
    assert!(keys.iter().all(|key| key == KEY));
}

#[test]
fn migration_previews_match_wire_and_bind_expire_and_consume_tokens() {
    with_fixture(Fixture::new, async |fixture| {
        let body = json!({"bundle":STANDARD.encode(bundle("direct")),"password":PASSWORD});
        let direct = post(fixture, "import", body.clone(), 200).await;
        assert_eq!(direct["status"], "ok");
        assert_imported(fixture, &["direct"]);
        let with_token = json!({"token":null,"bundle":body["bundle"],"password":PASSWORD});
        assert_eq!(
            post(fixture, "import", with_token, 400).await["error"]["message"],
            "Reopen the migration package"
        );

        let body = json!({"bundle":STANDARD.encode(bundle("selected")),"password":PASSWORD});
        let decoded = post(fixture, "decode", body, 200).await;
        assert_eq!(decoded["credential_providers"], json!(["selected"]));
        assert!(decoded["payload"].get("provider_keys").is_none());
        let input =
            json!({"token":token_field(&decoded,"token"),"selections":selection("selected")});
        let preview = post(fixture, "preview", input.clone(), 200).await;
        let selected = preview["config"]["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|provider| provider["id"] == "selected")
            .unwrap();
        assert!(selected["_shadow_migration_token"] == input["token"]);
        assert_eq!(selected["api_key_set"], true);
        assert_imported(fixture, &["direct"]);
        assert_eq!(
            post(fixture, "import", input.clone(), 200).await["status"],
            "ok"
        );
        assert_imported(fixture, &["direct", "selected"]);
        assert_eq!(
            post(fixture, "import", input, 400).await["error"]["message"],
            "Reopen the migration package"
        );

        let demo = post(fixture, "demo", json!({}), 200).await;
        let input = json!({"token":token_field(&demo,"token"),"selections":selection("direct")});
        assert_eq!(
            post(fixture, "import", input.clone(), 400).await["error"]["message"],
            "Demo packages cannot be imported"
        );
        let previews = &fixture.server.state.backend.migration_previews;
        previews.expire_for_test(input["token"].as_str().unwrap());
        assert_eq!(
            post(fixture, "preview", input, 400).await["error"]["message"],
            "Reopen the migration package"
        );
        assert_eq!(previews.len_for_test(), 0);

        let demo = post(fixture, "demo", json!({}), 200).await;
        let input = json!({"token":token_field(&demo,"token"),"selections":selection("direct")});
        assert!(fixture.server.state.sessions.rotate(system_now()).is_some());
        assert_eq!(
            post(fixture, "preview", input, 400).await["error"]["message"],
            "Reopen the migration package"
        );
    });
}

#[test]
fn migration_lan_writes_match_wire_through_loopback_delivery_and_cancel() {
    with_fixture(source, async |fixture| {
        let start = json!({
            "confirmation":confirmation(fixture).await, "address":"127.0.0.1", "selections":selection("source"),
        });
        let waiting = post(fixture, "lan/start", start.clone(), 200).await;
        assert_eq!(waiting["status"], "waiting");
        let code = waiting["code"].as_str().unwrap();
        let result = post(fixture, "lan/start", start, 403).await;
        assert_eq!(result["error"]["code"], "export_confirmation_required");
        assert_eq!(
            result["error"]["message"],
            "export confirmation is required"
        );

        let receive = json!({"code":code,"receiver":"42".repeat(32)});
        let result = post(fixture, "lan/receive", receive.clone(), 200).await;
        assert!(
            result == json!({"status":"waiting"}),
            "receiver must wait for approval"
        );
        let status = post(fixture, "lan/status", json!({}), 200).await;
        assert_eq!(status["status"], "confirmation");
        assert_eq!(status["receiver"], "127.0.0.1");
        assert!(status["code"].as_str() == Some(code));
        let result = post(fixture, "lan/approve", json!({"code":code}), 200).await;
        assert_eq!(result["status"], "confirmation");
        let received = post(fixture, "lan/receive", receive, 200).await;
        assert_eq!(received["credential_providers"], json!(["source"]));
        let review =
            json!({"token":token_field(&received,"token"),"selections":selection("source")});
        let result = post(fixture, "preview", review, 200).await;
        assert_eq!(result["config"]["providers"][0]["id"], "source");
        assert_eq!(result["config"]["providers"][0]["api_key_set"], true);
        assert_eq!(
            post(fixture, "lan/cancel", json!({}), 200).await["status"],
            "cancelled"
        );

        let start = json!({
            "confirmation":confirmation(fixture).await, "address":"127.0.0.1", "selections":selection("source"),
        });
        let waiting = post(fixture, "lan/start", start, 200).await;
        assert_eq!(waiting["status"], "waiting");
        let code = waiting["code"].as_str().unwrap();
        assert_eq!(
            post(fixture, "lan/cancel", json!({}), 200).await["status"],
            "cancelled"
        );
        assert_eq!(
            post(fixture, "lan/status", json!({}), 200).await["status"],
            "idle"
        );
        let receive = json!({"code":code,"receiver":"42".repeat(32)});
        assert_eq!(
            post(fixture, "lan/receive", receive, 400).await["error"]["message"],
            "lan_unreachable"
        );
        assert_eq!(
            post(fixture, "lan/receive", json!({"code":"invalid"}), 400).await["error"]["message"],
            "lan_invalid_code"
        );
        assert_eq!(
            post(fixture, "lan/approve", json!({"code":"invalid"}), 400).await["error"]["message"],
            "lan_expired"
        );
    });
}

async fn malformed(fixture: &Fixture, operation: &str, authenticated: bool, status: u16) -> Value {
    json_body(&exchange(fixture, operation, authenticated, b"[", status).await)
}

#[test]
fn migration_writes_match_wire_and_check_drain_session_and_team_before_body() {
    team_writes::run_team(async |fixture, upstream, base| {
        for operation in OPERATIONS {
            let unauthenticated = malformed(fixture, operation, false, 401).await;
            assert_eq!(
                unauthenticated["error"]["message"],
                "management session is required"
            );
            let invalid = malformed(fixture, operation, true, 400).await;
            assert!(
                invalid["error"]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("request body must be valid JSON:")
            );
        }
        team_writes::login(fixture, upstream, base).await;
        let unauthenticated = malformed(fixture, "lan/status", false, 401).await;
        assert_eq!(
            unauthenticated["error"]["message"],
            "management session is required"
        );

        let mut downgraded = team_writes::identity("Refreshed team");
        downgraded["member"]["role"] = json!("member");
        let mut client = TcpStream::connect(fixture.server.local_addr())
            .await
            .unwrap();
        let wire = wire(fixture, "lan/status", true, b"[");
        client.write_all(&wire[..wire.len() - 1]).await.unwrap();
        team_writes::respond(upstream, "status", None, 200, &downgraded).await;
        let mut reply = Vec::new();
        timeout(
            test_timeout(Duration::from_secs(5)),
            client.read_to_end(&mut reply),
        )
        .await
        .unwrap()
        .unwrap();
        assert_status(&reply, 403);
        assert_eq!(
            json_body(&reply)["error"]["code"],
            "workspace_migration_permission_required"
        );
        let profile = team_writes::read_team_profile(fixture);
        assert!(profile["session"]["identity"] == downgraded);

        for operation in OPERATIONS {
            let denied = malformed(fixture, operation, true, 403).await;
            assert_eq!(
                denied["error"]["code"],
                "workspace_migration_permission_required"
            );
        }
        {
            let state = fixture.server.state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(
                    state
                        .lifecycle
                        .updates()
                        .drain_for_test(test_timeout(Duration::from_secs(5)))
                );
            })
            .await
            .unwrap();
        }
        for operation in OPERATIONS {
            let updating = malformed(fixture, operation, false, 503).await;
            assert_eq!(updating["error"]["code"], "updating");
        }
    });
}

#[test]
fn submitted_migration_import_finishes_once_after_disconnect_and_holds_drain() {
    use crate::tests::{live_journal, wait_for};

    let body =
        json!({"bundle":STANDARD.encode(bundle("detached")),"password":PASSWORD}).to_string();
    with_fixture(Fixture::new, async |fixture| {
        {
            let state = &fixture.server.state;
            let (mut client, task) = connect_submitted_write(fixture).await;
            let wire = wire(fixture, "import", true, body.as_bytes());
            client
                .write_all(&wire[..wire.len() - body.len()])
                .await
                .unwrap();
            let admitted = state.clone();
            tokio::task::spawn_blocking(move || {
                wait_for(
                    &Deadline::after(test_timeout(Duration::from_secs(5))),
                    || admitted.lifecycle.updates().active_for_test() == 1,
                );
            })
            .await
            .unwrap();
            let (locked, ready) = tokio::sync::oneshot::channel();
            let (release, unlocked) = tokio::sync::oneshot::channel();
            let gated = state.clone();
            let hold_config = tokio::task::spawn_blocking(move || {
                let _config = gated.backend.configuration.test_config().lock().unwrap();
                locked.send(()).unwrap();
                let _ = unlocked.blocking_recv();
            });
            ready.await.unwrap();
            client.write_all(body.as_bytes()).await.unwrap();
            let root = fixture.directory.path().to_owned();
            let commit_started = |record: &Value| {
                record["event"] == "operation_stage_started"
                    && record["fields"]["operation"] == "migration_import"
                    && record["fields"]["last_stage"] == "commit_bundle"
            };
            let gated_root = root.clone();
            tokio::task::spawn_blocking(move || {
                wait_for(
                    &Deadline::after(test_timeout(Duration::from_secs(10))),
                    || live_journal(&gated_root).iter().any(commit_started),
                );
            })
            .await
            .unwrap();
            assert!(!state.lifecycle.updates().drain_for_test(Duration::ZERO));
            disconnect_submitted_write(fixture, client, task).await;
            release.send(()).unwrap();
            hold_config.await.unwrap();
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(
                    state
                        .lifecycle
                        .updates()
                        .drain_for_test(test_timeout(Duration::from_secs(5)))
                );
                assert!(
                    state
                        .lifecycle
                        .admission
                        .quiesce(0, Duration::from_secs(5))
                        .is_some()
                );
            })
            .await
            .unwrap();
            let records = live_journal(&root);
            assert_eq!(
                records
                    .iter()
                    .filter(|record| commit_started(record))
                    .count(),
                1
            );
            assert!(records.iter().any(|record| {
                record["event"] == "operation_finished"
                    && record["fields"]["operation"] == "migration_import"
                    && record["fields"]["outcome"] == "completed"
                    && record["fields"]["checks"]["catalog_published"] == true
            }));
            assert!(!records.iter().any(|record| {
                let text = record.to_string();
                text.contains(KEY) || text.contains(PASSWORD)
            }));
        }
        assert_imported(fixture, &["detached"]);
    });
}
