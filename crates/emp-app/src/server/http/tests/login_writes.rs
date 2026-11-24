use super::{
    account_writes::{Pair, normalize_body},
    *,
};
use emp_codex::configuration::local_auth;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const NATIVE: [&str; 3] = [
    "/api/native/login/start",
    "/api/native/login/status",
    "/api/native/login/cancel",
];
const ACCOUNTS: [&str; 3] = [
    "/api/accounts/login/start",
    "/api/accounts/login/status",
    "/api/accounts/login/cancel",
];
const USE_API_KEY: &str = "/api/native/login/use-api-key";
const USE_ACCOUNT: &str = "/api/native/login/use-account";
const USE_LOCAL_KEY: &str = "/api/native/login/use-local-key";
const BODY_LIMIT: &str = "Content-Length: 5242881";

#[test]
fn session_exchange_uses_admission_time_while_blocking_worker_is_queued() {
    let fixture = Fixture::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
        .block_on(async {
            let server = ServerHandle::bind(LOOPBACK, fixture.router(), admission())
                .await
                .unwrap();
            let state = &fixture.legacy.state;
            let url = state.reopen_url().unwrap();
            let header = format!(
                "X-EMP-Bootstrap: {}",
                url.split_once("?bootstrap=").unwrap().1
            );
            let (admitted, ready) = tokio::sync::oneshot::channel();
            let (release, unlocked) = tokio::sync::oneshot::channel();
            let gated = state.clone();
            let hold_worker = tokio::task::spawn_blocking(move || {
                crate::tests::wait_for(Duration::from_secs(5), || {
                    gated.lifecycle.updates().active_for_test() == 1
                });
                admitted.send(()).unwrap();
                unlocked.blocking_recv().unwrap();
            });
            let mut client = TcpStream::connect(server.local_addr()).await.unwrap();
            let wire = request(state.port, "POST", SESSION_PATH, &[&header], b"");
            client.write_all(&wire).await.unwrap();
            ready.await.unwrap();

            let expired_at = system_now();
            state
                .reopen_bootstrap
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .expires_at = expired_at;
            release.send(()).unwrap();
            hold_worker.await.unwrap();
            let mut reply = Vec::new();
            timeout(Duration::from_secs(5), client.read_to_end(&mut reply))
                .await
                .unwrap()
                .unwrap();
            assert_status(&reply, 200);
            let token = json_body(&reply)["session"].as_str().unwrap().to_owned();
            assert_eq!(fixture.legacy.session_token(), token);
            server.shutdown().await.unwrap();
        });
    fixture.legacy.shutdown().unwrap();
}

#[test]
fn session_exchange_matches_legacy_wire_and_rotates_once() {
    let mut results = Vec::new();
    for driver in wire_contract::Driver::ALL {
        let replies = Fixture::with_driver(tempfile::tempdir().unwrap(), driver).run_wire(
            async |fixture, address| {
                let port = address.port();
                let mut replies = Vec::new();
                for headers in [
                    vec![],
                    vec!["X-EMP-Bootstrap: wrong"],
                    vec!["X-EMP-Bootstrap: wrong", "X-EMP-Bootstrap: wrong"],
                ] {
                    let wire = request(port, "POST", SESSION_PATH, &headers, b"");
                    let reply = assert_wire(address, &wire, false, 401).await;
                    assert_eq!(
                        json_body(&reply)["error"]["message"],
                        "management session is required"
                    );
                    replies.push(reply);
                }

                let previous_session = fixture.session();
                let header = format!("X-EMP-Bootstrap: {}", fixture.legacy.state.bootstrap.token);
                let bootstrap = request(
                    port,
                    "POST",
                    "/api/session?unused=1",
                    &[&header, BODY_LIMIT],
                    b"{",
                );
                replies.push(
                    assert_wire_with(address, &bootstrap, false, 200, |reply| {
                        assert_rotated_session(fixture, reply)
                    })
                    .await,
                );
                replies.push(assert_wire(address, &bootstrap, false, 401).await);
                let stale = request(port, "GET", "/api/accounts", &[&previous_session], b"");
                replies.push(assert_wire(address, &stale, false, 401).await);
                let accounts = request(port, "GET", "/api/accounts", &[&fixture.session()], b"");
                let reply = assert_wire(address, &accounts, false, 200).await;
                assert_eq!(json_body(&reply)["accounts"], json!([]));
                replies.push(reply);

                let url = fixture.legacy.state.reopen_url().unwrap();
                let header = format!(
                    "X-EMP-Bootstrap: {}",
                    url.split_once("?bootstrap=").unwrap().1
                );
                drain_updates(fixture).await;
                let wire = request(port, "POST", SESSION_PATH, &[&header], b"");
                let reply = assert_wire(address, &wire, false, 503).await;
                assert_eq!(json_body(&reply)["error"]["code"], "updating");
                replies.push(reply);
                assert!(
                    !fixture
                        .legacy
                        .state
                        .reopen_bootstrap
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .used
                        .load(Ordering::Acquire)
                );
                replies.push(assert_wire(address, &accounts, false, 200).await);
                replies
            },
        );
        results.push(replies);
    }
    for pair in results.windows(2) {
        assert_eq!(pair[0], pair[1]);
    }
}

fn assert_rotated_session(fixture: &Fixture, reply: Vec<u8>) -> Vec<u8> {
    let body = json_body(&reply);
    let token = body["session"].as_str().unwrap();
    assert_eq!(token.len(), emp_state::WEB_SESSION_TOKEN_LENGTH);
    assert!(body["expires_in"].as_u64().unwrap() > 0);
    let wire = String::from_utf8_lossy(&reply);
    assert!(wire.contains(crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE));
    assert_eq!(fixture.legacy.session_token(), token);
    assert_eq!(
        read_json(&fixture.legacy.state.sessions.path)["token"],
        token
    );
    normalize_body(reply, |body| body.replace(token, "fixture-session"))
}

async fn drain_updates(fixture: &Fixture) {
    let state = fixture.legacy.state.clone();
    assert!(
        tokio::task::spawn_blocking(move || state
            .lifecycle
            .updates()
            .drain_for_test(Duration::from_secs(5)))
        .await
        .unwrap()
    );
}

fn login_fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = root.join("config.json");
    let catalog = root.join("native.json");
    std::fs::write(
        &catalog,
        br#"{"models":[{"slug":"fixture-native","visibility":"list","supported_in_api":true}]}"#,
    )
    .unwrap();
    std::fs::write(
        &config,
        json!({
            "auto_enable_on_start": false,
            "codex_base_url": "http://127.0.0.1:1/v1",
            "native_catalog_path": catalog,
            "account_store_path": root.join("accounts")
        })
        .to_string(),
    )
    .unwrap();
    let helper = root.join("codex-fixture");
    std::fs::write(
        &helper,
        include_bytes!("../../../../../../scripts/shadow_frontend/fixtures/device-codex.py"),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let legacy = LegacyServer::start_with_config_options(
        LOOPBACK.ip(),
        0,
        &config,
        helper.to_str().unwrap(),
        root.join("codex/auth.json"),
    )
    .unwrap();
    Fixture {
        directory,
        legacy,
        upstream: Arc::default(),
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn accounts(fixture: &Fixture) -> Vec<Value> {
    let config = read_json(&fixture.directory.path().join("config.json"));
    config["accounts"].as_array().cloned().unwrap_or_default()
}

fn assert_one_saved_account(
    pair: &Pair<'_>,
    name: &str,
    source: Value,
    token: &str,
) -> [String; 2] {
    let [(old_id, old_account), (new_id, new_account)] = [pair.old, pair.new].map(|fixture| {
        let [mut account]: [Value; 1] = accounts(fixture).try_into().unwrap();
        assert_eq!(account["name"], name);
        assert_eq!(account["credential_source"], source);
        let id = account["id"].as_str().unwrap().to_owned();
        let dir = fixture
            .directory
            .path()
            .canonicalize()
            .unwrap()
            .join("accounts")
            .join(&id);
        let auth_path = PathBuf::from(account["auth_file"].as_str().unwrap());
        assert_eq!(auth_path, dir.join("auth.json.enc"));
        let auth = fixture
            .legacy
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&auth_path)
            .unwrap();
        assert_eq!(auth["tokens"]["access_token"], token);
        assert_eq!(
            std::fs::read(dir.join("config.toml")).unwrap(),
            b"cli_auth_credentials_store = \"file\"\n"
        );
        account["id"] = json!("account-fixture");
        assert_eq!(account["prefix"], id);
        account["prefix"] = json!("account-fixture");
        account["auth_file"] = json!("accounts/account-fixture/auth.json.enc");
        (id, account)
    });
    assert_eq!(old_account, new_account);
    [old_id, new_id]
}

fn native_auth(fixture: &Fixture) -> &Path {
    &fixture.legacy.state.backend.accounts.native_auth_path
}

async fn conflict(pair: &Pair<'_>, path: &str, body: &[u8], code: &str) {
    let reply = pair
        .parity(
            |port, session| json_request(port, path, &[session], body),
            409,
            |reply| {
                assert!(reply.starts_with(b"HTTP/1.1 409 Conflict\r\n"));
                reply
            },
        )
        .await;
    assert_eq!(reply["error"]["code"], code);
    assert_eq!(reply["error"]["message"], code);
}

#[test]
fn login_writes_match_legacy_wire_and_switch_native_credentials() {
    let old = login_fixture();
    login_fixture().run_candidate(async |new, candidate| {
        let pair = Pair {
            old: &old,
            new,
            candidate,
        };
        let routes =
            NATIVE
                .into_iter()
                .chain(ACCOUNTS)
                .chain([USE_API_KEY, USE_ACCOUNT, USE_LOCAL_KEY]);
        for path in routes.clone() {
            pair.parity(
                |port, _| json_request(port, path, &[], b"{"),
                401,
                |reply| reply,
            )
            .await;
            pair.parity(
                |port, session| {
                    json_request(
                        port,
                        path,
                        &[session, "Origin: https://example.invalid"],
                        b"{",
                    )
                },
                403,
                |reply| reply,
            )
            .await;
            assert!(
                pair.post(path, b"{", 400).await["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("valid JSON")
            );
            assert_eq!(
                pair.post(path, b"[]", 400).await["error"]["message"],
                "request body must be a JSON object"
            );
            let denied = pair
                .parity(
                    |port, session| {
                        request(
                            port,
                            "POST",
                            path,
                            &[session, "Content-Type: application/json", BODY_LIMIT],
                            b"",
                        )
                    },
                    413,
                    |reply| reply,
                )
                .await;
            assert_eq!(denied["error"]["limit_bytes"], 5242880);
        }
        let oversized = gzip(&vec![b' '; 5 * 1024 * 1024 + 1], Vec::new());
        pair.parity(
            |port, session| {
                json_request(
                    port,
                    NATIVE[0],
                    &[session, "Content-Encoding: gzip"],
                    &oversized,
                )
            },
            413,
            |reply| reply,
        )
        .await;

        for [start, status, cancel] in [NATIVE, ACCOUNTS] {
            conflict(
                &pair,
                start,
                br#"{"method":"unknown"}"#,
                "login_invalid_method",
            )
            .await;
            assert_eq!(
                pair.post(status, br#"{"ignored":null}"#, 200).await,
                json!({"status":"idle"})
            );
            assert_eq!(
                pair.post(cancel, br#"{"ignored":null}"#, 200).await,
                json!({"status":"cancelled"})
            );
        }
        for body in [b"{}".as_slice(), br#"{"name":7}"#] {
            conflict(&pair, ACCOUNTS[0], body, "login_name_required").await;
        }
        conflict(
            &pair,
            ACCOUNTS[0],
            br#"{"name":"a/b"}"#,
            "login_name_contains_slash",
        )
        .await;
        for path in [
            "/api/accounts/login/use-api-key",
            "/api/accounts/login/use-account",
            "/api/accounts/login/use-local-key",
            "/api/native/login/",
            "/api/native/login/%73tatus",
        ] {
            conflict(&pair, path, b"{}", "login_unknown_operation").await;
        }
        assert_eq!(
            pair.post("/api/accounts/login/legacy/status?unused=1", b"{}", 200)
                .await["status"],
            "cancelled"
        );

        for (path, wrong_type, code) in [
            (
                USE_API_KEY,
                br#"{"api_key":3}"#.as_slice(),
                "login_invalid_api_key",
            ),
            (
                USE_ACCOUNT,
                br#"{"account_id":3}"#,
                "login_account_required",
            ),
            (
                USE_LOCAL_KEY,
                br#"{"account_action":3}"#,
                "login_invalid_action",
            ),
        ] {
            conflict(&pair, path, b"{}", code).await;
            conflict(&pair, path, wrong_type, code).await;
        }
        conflict(
            &pair,
            USE_ACCOUNT,
            br#"{"account_id":"absent"}"#,
            "login_account_required",
        )
        .await;
        conflict(
            &pair,
            USE_LOCAL_KEY,
            br#"{"account_action":"keep"}"#,
            "login_auth_unreadable",
        )
        .await;

        let original =
            json!({"tokens":{"account_id":"original","access_token":"fixture-original"}});
        for fixture in [&old, new] {
            emp_state::atomic_write_private_state(
                native_auth(fixture),
                &serde_json::to_vec(&original).unwrap(),
            )
            .unwrap();
        }
        let models = pair.read("/v1/models").await;
        assert!(
            models["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "fixture-native")
        );

        assert_eq!(
            pair.post(
                USE_LOCAL_KEY,
                br#"{"account_action":"keep","ignored":true}"#,
                200
            )
            .await["integration_updated"],
            true
        );
        let ids = assert_one_saved_account(&pair, "Native", Value::Null, "fixture-original");
        for (fixture, id) in [&old, new].into_iter().zip(&ids) {
            assert_eq!(local_auth::mode(native_auth(fixture)), "local_key");
            assert_eq!(
                read_json(native_auth(fixture))["OPENAI_API_KEY"],
                fixture.legacy.state.backend.gateway_key
            );
            let retained = read_json(
                &fixture
                    .directory
                    .path()
                    .join("state/local-key-subscription.json"),
            );
            assert_eq!(retained["status"], "kept");
            assert_eq!(retained["account_id"], *id);
        }
        let local_models = pair.read("/v1/models").await;
        assert!(
            !local_models["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|model| model["id"] == "fixture-native")
        );

        let restored = pair
            .parity(
                |port, session| {
                    let id = if port == old.port() { &ids[0] } else { &ids[1] };
                    let body = json!({"account_id":id,"ignored":true}).to_string();
                    json_request(port, USE_ACCOUNT, &[session], body.as_bytes())
                },
                200,
                |reply| reply,
            )
            .await;
        assert_eq!(restored["status"], "completed");
        assert_eq!(pair.read("/v1/models").await, models);
        for fixture in [&old, new] {
            assert_eq!(
                read_json(native_auth(fixture))["tokens"],
                original["tokens"]
            );
            emp_state::atomic_write_private_state(
                native_auth(fixture),
                br#"{"tokens":{"account_id":"discarded","access_token":"fixture-discarded"}}"#,
            )
            .unwrap();
        }

        assert_eq!(
            pair.post(USE_LOCAL_KEY, br#"{"account_action":"remove"}"#, 200)
                .await["status"],
            "completed"
        );
        assert_eq!(
            assert_one_saved_account(&pair, "Native", Value::Null, "fixture-original"),
            ids
        );
        for fixture in [&old, new] {
            assert_eq!(local_auth::mode(native_auth(fixture)), "local_key");
            assert_eq!(
                read_json(
                    &fixture
                        .directory
                        .path()
                        .join("state/local-key-subscription.json")
                ),
                json!({"status":"removed"})
            );
        }
        assert_eq!(pair.read("/v1/models").await, local_models);

        assert_eq!(
            pair.post(
                USE_API_KEY,
                br#"{"api_key":" fixture-api-key ","ignored":1}"#,
                200
            )
            .await["restart_required"],
            true
        );
        for fixture in [&old, new] {
            assert_eq!(
                read_json(native_auth(fixture)),
                json!({"auth_mode":"apikey","OPENAI_API_KEY":"fixture-api-key"})
            );
            drain_updates(fixture).await;
        }
        for path in routes {
            let denied = pair
                .parity(
                    |port, _| json_request(port, path, &[], b"{"),
                    503,
                    |reply| reply,
                )
                .await;
            assert_eq!(denied["error"]["code"], "updating");
        }
        assert_eq!(
            assert_one_saved_account(&pair, "Native", Value::Null, "fixture-original"),
            ids
        );
        assert_eq!(pair.read("/v1/models").await, models);
    });
    old.legacy.shutdown().unwrap();
}

async fn wait_for_login(address: SocketAddr, wire: Vec<u8>, expected: &'static str) {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        crate::tests::wait_for(Duration::from_secs(10), || {
            let reply = runtime.block_on(exchange_wire(address, &wire, false));
            assert_status(&reply, 200);
            let status = json_body(&reply);
            assert_ne!(status["status"], "failed", "{status}");
            status["status"] == expected
        });
    })
    .await
    .unwrap();
}

fn pending_wire(reply: Vec<u8>) -> Vec<u8> {
    assert_eq!(
        String::from_utf8_lossy(&reply)
            .matches("Cache-Control: no-store\r\n")
            .count(),
        2
    );
    let expiration = json_body(&reply)["expires_at"].as_u64().unwrap();
    assert!(expiration > system_now() as u64);
    normalize_body(reply, |body| {
        body.replace(&format!("\"expires_at\":{expiration}"), "\"expires_at\":0")
    })
}

#[test]
fn submitted_login_survives_disconnect_and_completion_is_applied_once() {
    let old = login_fixture();
    login_fixture().run_candidate(async |new, candidate| {
        let pair = Pair {
            old: &old,
            new,
            candidate,
        };
        let servers = [(&old, old.legacy.local_addr()), (new, candidate)];
        for [start, status, cancel] in [NATIVE, ACCOUNTS] {
            let original_auth =
                [&old, new].map(|fixture| local_auth::read(native_auth(fixture)).unwrap());
            for (fixture, address) in servers {
                let body = br#"{"method":"browser","name":"Cancelled fixture"}"#;
                let reply = exchange_wire(
                    address,
                    &json_request(fixture.port(), start, &[&fixture.session()], body),
                    false,
                )
                .await;
                assert_status(&reply, 200);
                wait_for_login(
                    address,
                    json_request(fixture.port(), status, &[&fixture.session()], b"{}"),
                    "pending",
                )
                .await;
            }
            let replay = pair
                .parity(
                    |port, session| {
                        json_request(
                            port,
                            start,
                            &[session],
                            br#"{"method":"ignored while pending"}"#,
                        )
                    },
                    200,
                    pending_wire,
                )
                .await;
            assert_eq!(replay["method"], "browser");
            assert_eq!(pair.post(cancel, b"{}", 200).await["status"], "cancelled");
            for fixture in [&old, new] {
                std::fs::write(fixture.directory.path().join("login-control"), "complete").unwrap();
            }
            assert_eq!(pair.post(status, b"{}", 200).await["status"], "cancelled");
            assert_eq!(
                [&old, new].map(|fixture| local_auth::read(native_auth(fixture)).unwrap()),
                original_auth
            );
            for fixture in [&old, new] {
                assert!(accounts(fixture).is_empty());
                std::fs::remove_file(fixture.directory.path().join("login-control")).unwrap();
            }

            let importing = start == ACCOUNTS[0];
            let mut clients = Vec::new();
            for (fixture, address) in servers {
                let mut client = TcpStream::connect(address).await.unwrap();
                let body = br#"{"method":false,"name":" Fixture login ","ignored":42}"#;
                client
                    .write_all(&json_request(
                        fixture.port(),
                        start,
                        &[&fixture.session()],
                        body,
                    ))
                    .await
                    .unwrap();
                wait_for_login(
                    address,
                    json_request(fixture.port(), status, &[&fixture.session()], b"{}"),
                    "pending",
                )
                .await;
                clients.push(client);
            }
            let pending = pair
                .parity(
                    |port, session| json_request(port, status, &[session], b"{}"),
                    200,
                    pending_wire,
                )
                .await;
            assert_eq!(pending["method"], "device");
            assert_eq!(pending["user_code"], "ABCD-EFGH");
            if !importing {
                for path in [USE_API_KEY, USE_ACCOUNT, USE_LOCAL_KEY] {
                    conflict(&pair, path, b"{}", "login_busy").await;
                }
            }
            for client in clients {
                client.set_zero_linger().unwrap();
            }
            for (fixture, address) in servers {
                std::fs::write(fixture.directory.path().join("login-control"), "complete").unwrap();
                wait_for_login(
                    address,
                    json_request(fixture.port(), status, &[&fixture.session()], b"{}"),
                    "completed",
                )
                .await;
            }

            let ids = importing.then(|| {
                assert_one_saved_account(
                    &pair,
                    "Fixture login",
                    json!("emp_login"),
                    "fixture-access",
                )
            });
            let written = |fixture: &Fixture, index: usize| {
                let auth = native_auth(fixture).to_path_buf();
                let mut paths = vec![auth];
                if let Some(ids) = &ids {
                    paths.push(fixture.directory.path().join("config.json"));
                    paths.push(
                        fixture
                            .directory
                            .path()
                            .join("accounts")
                            .join(&ids[index])
                            .join("auth.json.enc"),
                    );
                }
                paths
                    .into_iter()
                    .map(|path| {
                        (
                            std::fs::metadata(&path).unwrap().ino(),
                            std::fs::read(path).unwrap(),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let before = [written(&old, 0), written(new, 1)];
            assert_eq!(before[0][0].1, before[1][0].1);
            let native = read_json(native_auth(&old));
            assert_eq!(native["tokens"]["access_token"], "fixture-access");
            assert_eq!(native["tokens"]["account_id"], "fixture-login");
            let done = pair.post(status, b"{}", 200).await;
            assert_eq!(done["status"], "completed");
            assert_eq!(done["restart_required"], !importing);
            assert_eq!(pair.post(status, b"{}", 200).await, done);
            assert_eq!([written(&old, 0), written(new, 1)], before);
            for fixture in [&old, new] {
                std::fs::remove_file(fixture.directory.path().join("login-control")).unwrap();
            }
        }
    });
    old.legacy.shutdown().unwrap();
}
