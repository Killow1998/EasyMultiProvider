use super::{account_writes::Pair, team_writes, *};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
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

fn endpoints<'a>(pair: &Pair<'a>) -> [(&'a Fixture, SocketAddr); 2] {
    [
        (pair.old, pair.old.legacy.local_addr()),
        (pair.new, pair.candidate),
    ]
}

fn pair_with(make: impl Fn() -> Fixture, test: impl AsyncFnOnce(&Pair<'_>)) {
    let old = make();
    make().run_candidate(async |new, candidate| {
        test(&Pair {
            old: &old,
            new,
            candidate,
        })
        .await;
    });
    old.legacy.shutdown().unwrap();
}

fn provider(id: &str) -> Value {
    json!({"id":id,"name":id,"base_url":format!("https://{id}.example.invalid/v1"),"protocol":"responses","auth_mode":"api_key"})
}

fn model(id: &str) -> Value {
    json!({"id":format!("{id}/model"),"provider":id,"upstream_id":"model","enabled":true})
}

fn source() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
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

fn replace_body(head: &str, length: usize, body: &[u8]) -> Vec<u8> {
    let head = head.replace(
        &format!("Content-Length: {length}\r\n"),
        &format!("Content-Length: {}\r\n", body.len()),
    );
    [head.as_bytes(), b"\r\n", body].concat()
}

fn validate_bundle(bundle: &[u8]) {
    let envelope = bundle
        .strip_prefix(emp_state::migration::MIGRATION_MAGIC)
        .expect("migration bundle magic");
    assert!(envelope.ends_with(b"\n"));
    let envelope: Value = serde_json::from_slice(envelope).expect("migration envelope");
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

fn normalize(fixture: &Fixture, operation: &str, reply: &[u8]) -> Vec<u8> {
    let (head, body) = split_wire(reply);
    if head.starts_with("HTTP/1.1 200 ")
        && head.contains("Content-Type: application/octet-stream\r\n")
    {
        validate_bundle(body);
        return replace_body(head, body.len(), &vec![0; body.len()]);
    }
    let value: Value = serde_json::from_slice(body).expect("migration response JSON");
    let mut normalized = std::str::from_utf8(body).unwrap().to_owned();
    if let Some(confirmation) = value["confirmation"].as_str() {
        assert_eq!(confirmation.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(confirmation).unwrap().len(), 32);
        normalized = normalized.replace(confirmation, &"c".repeat(43));
    }
    if value.get("token").is_some() {
        normalized = normalized.replace(&token_field(&value, "token"), &"a".repeat(48));
    }
    if operation == "preview"
        && let Some(providers) = value["config"]["providers"].as_array()
    {
        for provider in providers
            .iter()
            .filter(|row| row.get("_shadow_migration_token").is_some())
        {
            normalized = normalized.replace(
                &token_field(provider, "_shadow_migration_token"),
                &"a".repeat(48),
            );
        }
    }
    if let Some(code) = value["code"].as_str() {
        validate_code(code);
        normalized = normalized.replace(code, &"e".repeat(49));
        let remaining = value["remaining_seconds"].as_u64().unwrap();
        assert!((1..=300).contains(&remaining));
        normalized = normalized.replace(
            &format!("\"remaining_seconds\":{remaining}"),
            "\"remaining_seconds\":0",
        );
    }
    if let Some(encoded) = value["bundle"].as_str() {
        validate_bundle(&STANDARD.decode(encoded).unwrap());
        normalized = normalized.replace(encoded, &"b".repeat(encoded.len()));
    }
    let root = fixture.directory.path().canonicalize().unwrap();
    normalized = normalized.replace(root.to_str().unwrap(), "/fixture");
    assert!(!normalized.contains(KEY) && !normalized.contains(PASSWORD));
    replace_body(head, body.len(), normalized.as_bytes())
}

async fn assert_migration_wire_parity(
    pair: &Pair<'_>,
    operation: &str,
    requests: [&[u8]; 2],
    status: u16,
) -> [Vec<u8>; 2] {
    let [(old, old_addr), (new, new_addr)] = endpoints(pair);
    let (old_reply, new_reply) = tokio::join!(
        exchange_wire(old_addr, requests[0], false),
        exchange_wire(new_addr, requests[1], false),
    );
    let expected = format!("HTTP/1.1 {status} ");
    assert!(
        old_reply.starts_with(expected.as_bytes()),
        "legacy {operation}: expected {status}"
    );
    assert!(
        new_reply.starts_with(expected.as_bytes()),
        "candidate {operation}: expected {status}"
    );
    assert!(
        normalize(old, operation, &old_reply) == normalize(new, operation, &new_reply),
        "wire response differs for {operation}"
    );
    [old_reply, new_reply]
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

async fn post_wire(
    pair: &Pair<'_>,
    operation: &str,
    bodies: [Value; 2],
    status: u16,
) -> [Vec<u8>; 2] {
    let [old_body, new_body] = bodies;
    let old = wire(pair.old, operation, true, old_body.to_string().as_bytes());
    let new = wire(pair.new, operation, true, new_body.to_string().as_bytes());
    assert_migration_wire_parity(pair, operation, [&old, &new], status).await
}

async fn post(pair: &Pair<'_>, operation: &str, bodies: [Value; 2], status: u16) -> [Value; 2] {
    post_wire(pair, operation, bodies, status)
        .await
        .map(|reply| json_body(&reply))
}

async fn same(pair: &Pair<'_>, operation: &str, body: Value, status: u16) -> Value {
    let [old, _] = post(pair, operation, [body.clone(), body], status).await;
    old
}

async fn confirmations(pair: &Pair<'_>) -> [String; 2] {
    post(pair, "export/confirm", [json!({}), json!({})], 200)
        .await
        .map(|value| {
            assert_eq!(value["expires_in"], 60);
            value["confirmation"].as_str().unwrap().to_owned()
        })
}

fn verify_payload(bundle: &[u8]) {
    let decrypted = emp_state::migration::decode_migration(PASSWORD, bundle).unwrap();
    let payload: Value = serde_json::from_slice(&decrypted).unwrap();
    assert!(payload["provider_keys"]["source"] == KEY);
    assert_eq!(payload["config"]["providers"][0]["id"], "source");
    assert_eq!(payload["config"]["models"][0]["id"], "source/model");
}

#[test]
fn migration_exports_match_wire_and_consume_confirmation_once() {
    pair_with(source, async |pair| {
        let bodies = confirmations(pair).await.map(|confirmation| {
            json!({
                "confirmation":confirmation, "password":PASSWORD, "groups":["external"],
            })
        });
        for reply in post_wire(pair, "export", bodies.clone(), 200).await {
            let (head, body) = split_wire(&reply);
            assert!(head.contains("Content-Type: application/octet-stream\r\n"));
            assert!(head.contains("Content-Disposition: attachment; filename=\"EMP.emp\"\r\n"));
            assert!(head.contains("X-EMP-Export-Summary: "));
            verify_payload(body);
        }
        for result in post(pair, "export", bodies, 403).await {
            assert_eq!(result["error"]["code"], "export_confirmation_required");
        }

        let selected = confirmations(pair).await.map(|confirmation| {
            json!({
                "confirmation":confirmation, "password":PASSWORD, "selections":selection("source"),
                "include_context":false,"include_rename":false,
            })
        });
        for value in post(pair, "export", selected, 200).await {
            verify_payload(&STANDARD.decode(value["bundle"].as_str().unwrap()).unwrap());
            assert_eq!(value["summary"]["providers"], 1);
        }

        let expired = [pair.old, pair.new].map(|fixture| {
            let confirmation = fixture
                .legacy
                .state
                .sessions
                .issue_export_confirmation(
                    crate::http::auth::EXPORT_CONFIRMATION_OPERATION,
                    system_now() - 61.0,
                )
                .unwrap();
            json!({"confirmation":confirmation,"password":PASSWORD})
        });
        for value in post(pair, "export", expired, 403).await {
            assert_eq!(value["error"]["code"], "export_confirmation_required");
        }
    });
}

fn saved_config(fixture: &Fixture) -> Value {
    let root = fixture.directory.path().canonicalize().unwrap();
    serde_json::from_slice(&fs::read(root.join("config.json")).unwrap()).unwrap()
}

fn assert_imported(pair: &Pair<'_>, ids: &[&str]) {
    let [old, new] = [pair.old, pair.new].map(|fixture| {
        let config = saved_config(fixture);
        let state = &fixture.legacy.state;
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
        let root = fixture.directory.path().canonicalize().unwrap();
        (
            config
                .to_string()
                .replace(root.to_str().unwrap(), "/fixture"),
            catalog,
            keys,
        )
    });
    assert!(
        old == new,
        "persisted migration config, credentials and catalog differ"
    );
}

#[test]
fn migration_previews_match_wire_and_bind_expire_and_consume_tokens() {
    pair_with(Fixture::new, async |pair| {
        let body = json!({"bundle":STANDARD.encode(bundle("direct")),"password":PASSWORD});
        let direct = same(pair, "import", body.clone(), 200).await;
        assert_eq!(direct["status"], "ok");
        assert_imported(pair, &["direct"]);
        let with_token = json!({"token":null,"bundle":body["bundle"],"password":PASSWORD});
        assert_eq!(
            same(pair, "import", with_token, 400).await["error"]["message"],
            "Reopen the migration package"
        );

        let body = json!({"bundle":STANDARD.encode(bundle("selected")),"password":PASSWORD});
        let decoded = post(pair, "decode", [body.clone(), body], 200).await;
        let inputs = decoded.map(|value| {
            assert_eq!(value["credential_providers"], json!(["selected"]));
            assert!(value["payload"].get("provider_keys").is_none());
            json!({"token":token_field(&value,"token"),"selections":selection("selected")})
        });
        for (preview, input) in post(pair, "preview", inputs.clone(), 200)
            .await
            .into_iter()
            .zip(&inputs)
        {
            let providers = preview["config"]["providers"].as_array().unwrap();
            let selected = providers
                .iter()
                .find(|provider| provider["id"] == "selected")
                .unwrap();
            assert!(selected["_shadow_migration_token"] == input["token"]);
            assert_eq!(selected["api_key_set"], true);
        }
        assert_imported(pair, &["direct"]);
        for result in post(pair, "import", inputs.clone(), 200).await {
            assert_eq!(result["status"], "ok");
        }
        assert_imported(pair, &["direct", "selected"]);
        for result in post(pair, "import", inputs, 400).await {
            assert_eq!(result["error"]["message"], "Reopen the migration package");
        }

        let demo = post(pair, "demo", [json!({}), json!({})], 200).await;
        let inputs = demo.map(
            |value| json!({"token":token_field(&value,"token"),"selections":selection("direct")}),
        );
        for result in post(pair, "import", inputs.clone(), 400).await {
            assert_eq!(
                result["error"]["message"],
                "Demo packages cannot be imported"
            );
        }
        for (fixture, input) in [pair.old, pair.new].into_iter().zip(&inputs) {
            let cache = &fixture.legacy.state.backend.migration_previews;
            cache.expire_for_test(input["token"].as_str().unwrap());
        }
        for result in post(pair, "preview", inputs, 400).await {
            assert_eq!(result["error"]["message"], "Reopen the migration package");
        }
        for (fixture, _) in endpoints(pair) {
            assert_eq!(
                fixture
                    .legacy
                    .state
                    .backend
                    .migration_previews
                    .len_for_test(),
                0
            );
        }

        let demo = post(pair, "demo", [json!({}), json!({})], 200).await;
        let inputs = demo.map(
            |value| json!({"token":token_field(&value,"token"),"selections":selection("direct")}),
        );
        for (fixture, _) in endpoints(pair) {
            assert!(fixture.legacy.state.sessions.rotate(system_now()).is_some());
        }
        for result in post(pair, "preview", inputs, 400).await {
            assert_eq!(result["error"]["message"], "Reopen the migration package");
        }
    });
}

#[test]
fn migration_lan_writes_match_wire_through_loopback_delivery_and_cancel() {
    pair_with(source, async |pair| {
        let start = confirmations(pair).await.map(|confirmation| json!({
            "confirmation":confirmation, "address":"127.0.0.1", "selections":selection("source"),
        }));
        let codes = post(pair, "lan/start", start.clone(), 200)
            .await
            .map(|value| {
                assert_eq!(value["status"], "waiting");
                value["code"].as_str().unwrap().to_owned()
            });
        for result in post(pair, "lan/start", start, 403).await {
            assert_eq!(result["error"]["code"], "export_confirmation_required");
            assert_eq!(
                result["error"]["message"],
                "export confirmation is required"
            );
        }
        let receive = codes
            .each_ref()
            .map(|code| json!({"code":code,"receiver":"42".repeat(32)}));
        for result in post(pair, "lan/receive", receive.clone(), 200).await {
            assert!(
                result == json!({"status":"waiting"}),
                "receiver must wait for approval"
            );
        }
        for (status, code) in post(pair, "lan/status", [json!({}), json!({})], 200)
            .await
            .into_iter()
            .zip(&codes)
        {
            assert_eq!(status["status"], "confirmation");
            assert_eq!(status["receiver"], "127.0.0.1");
            assert!(status["code"].as_str() == Some(code.as_str()));
        }
        for result in post(
            pair,
            "lan/approve",
            codes.each_ref().map(|code| json!({"code":code})),
            200,
        )
        .await
        {
            assert_eq!(result["status"], "confirmation");
        }
        let received = post(pair, "lan/receive", receive, 200).await;
        let review = received.map(|value| {
            assert_eq!(value["credential_providers"], json!(["source"]));
            json!({"token":token_field(&value,"token"),"selections":selection("source")})
        });
        for result in post(pair, "preview", review, 200).await {
            assert_eq!(result["config"]["providers"][0]["id"], "source");
            assert_eq!(result["config"]["providers"][0]["api_key_set"], true);
        }
        assert_eq!(
            same(pair, "lan/cancel", json!({}), 200).await["status"],
            "cancelled"
        );

        let start = confirmations(pair).await.map(|confirmation| json!({
            "confirmation":confirmation, "address":"127.0.0.1", "selections":selection("source"),
        }));
        let cancelled = post(pair, "lan/start", start, 200).await.map(|value| {
            assert_eq!(value["status"], "waiting");
            value["code"].as_str().unwrap().to_owned()
        });
        assert_eq!(
            same(pair, "lan/cancel", json!({}), 200).await["status"],
            "cancelled"
        );
        assert_eq!(
            same(pair, "lan/status", json!({}), 200).await["status"],
            "idle"
        );
        let receive = cancelled.map(|code| json!({"code":code,"receiver":"42".repeat(32)}));
        for result in post(pair, "lan/receive", receive, 400).await {
            assert_eq!(result["error"]["message"], "lan_unreachable");
        }
        assert_eq!(
            same(pair, "lan/receive", json!({"code":"invalid"}), 400).await["error"]["message"],
            "lan_invalid_code"
        );
        assert_eq!(
            same(pair, "lan/approve", json!({"code":"invalid"}), 400).await["error"]["message"],
            "lan_expired"
        );
    });
}

async fn malformed(pair: &Pair<'_>, operation: &str, authenticated: bool, status: u16) -> Value {
    let old = wire(pair.old, operation, authenticated, b"[");
    let new = wire(pair.new, operation, authenticated, b"[");
    let [reply, _] = assert_migration_wire_parity(pair, operation, [&old, &new], status).await;
    json_body(&reply)
}

#[test]
fn migration_writes_match_wire_and_check_drain_session_and_team_before_body() {
    team_writes::run_pair(async |pair, upstream, base| {
        for operation in OPERATIONS {
            let unauthenticated = malformed(pair, operation, false, 401).await;
            assert_eq!(
                unauthenticated["error"]["message"],
                "management session is required"
            );
            let invalid = malformed(pair, operation, true, 400).await;
            assert!(
                invalid["error"]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("request body must be valid JSON:")
            );
        }
        team_writes::login(pair, upstream, base).await;
        let unauthenticated = malformed(pair, "lan/status", false, 401).await;
        assert_eq!(
            unauthenticated["error"]["message"],
            "management session is required"
        );

        let mut downgraded = team_writes::identity("Refreshed team");
        downgraded["member"]["role"] = json!("member");
        let mut clients = Vec::new();
        for (fixture, address) in endpoints(pair) {
            let mut client = TcpStream::connect(address).await.unwrap();
            let wire = wire(fixture, "lan/status", true, b"[");
            client.write_all(&wire[..wire.len() - 1]).await.unwrap();
            clients.push(client);
        }
        team_writes::respond(upstream, "status", None, 200, &downgraded).await;
        let mut replies = Vec::new();
        for mut client in clients {
            let mut reply = Vec::new();
            timeout(Duration::from_secs(5), client.read_to_end(&mut reply))
                .await
                .unwrap()
                .unwrap();
            assert!(reply.starts_with(b"HTTP/1.1 403 "));
            assert_eq!(
                json_body(&reply)["error"]["code"],
                "workspace_migration_permission_required"
            );
            replies.push(reply);
        }
        assert!(
            replies[0] == replies[1],
            "Team refresh preceding the body differs"
        );
        for (fixture, _) in endpoints(pair) {
            let profile = team_writes::read_team_profile(fixture);
            assert!(profile["session"]["identity"] == downgraded);
        }

        for operation in OPERATIONS {
            let denied = malformed(pair, operation, true, 403).await;
            assert_eq!(
                denied["error"]["code"],
                "workspace_migration_permission_required"
            );
        }
        for (fixture, _) in endpoints(pair) {
            let state = fixture.legacy.state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(state.updates.drain_for_test(Duration::from_secs(5)));
            })
            .await
            .unwrap();
        }
        for operation in OPERATIONS {
            let updating = malformed(pair, operation, false, 503).await;
            assert_eq!(updating["error"]["code"], "updating");
        }
    });
}

#[test]
fn submitted_migration_import_finishes_once_after_disconnect_and_holds_drain() {
    use crate::tests::{live_journal, wait_for};

    let body =
        json!({"bundle":STANDARD.encode(bundle("detached")),"password":PASSWORD}).to_string();
    pair_with(Fixture::new, async |pair| {
        for (fixture, address) in endpoints(pair) {
            let state = &fixture.legacy.state;
            let mut client = TcpStream::connect(address).await.unwrap();
            let wire = wire(fixture, "import", true, body.as_bytes());
            client
                .write_all(&wire[..wire.len() - body.len()])
                .await
                .unwrap();
            let admitted = state.clone();
            tokio::task::spawn_blocking(move || {
                wait_for(Duration::from_secs(5), || {
                    admitted.updates.active_for_test() == 1
                });
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
                wait_for(Duration::from_secs(10), || {
                    live_journal(&gated_root).iter().any(commit_started)
                });
            })
            .await
            .unwrap();
            assert!(!state.updates.drain_for_test(Duration::ZERO));
            client.set_zero_linger().unwrap();
            drop(client);
            release.send(()).unwrap();
            hold_config.await.unwrap();
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                assert!(state.updates.drain_for_test(Duration::from_secs(5)));
                assert!(
                    state
                        .connection_admission
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
        assert_imported(pair, &["detached"]);
    });
}
