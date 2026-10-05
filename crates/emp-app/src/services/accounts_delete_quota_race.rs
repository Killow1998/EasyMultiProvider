#![cfg(unix)]

use crate::lifecycle::ServerHandle;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::{delete_account_state, import_account_state, quota_owner_key};

fn http_request(
    address: SocketAddr,
    method: &str,
    target: &str,
    cookie: &str,
    body: &[u8],
    sent: Option<mpsc::Sender<()>>,
) -> String {
    let mut stream = TcpStream::connect(address).expect("connect management endpoint");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .expect("set endpoint timeout");
    write!(
        stream,
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-EMP-Session: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address.port(),
        body.len()
    )
    .expect("write endpoint request head");
    stream.write_all(body).expect("write endpoint body");
    stream.flush().expect("flush endpoint request");
    if let Some(sent) = sent {
        sent.send(()).expect("signal endpoint request sent");
    }
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read endpoint response");
    String::from_utf8(response).expect("endpoint response is UTF-8")
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.is_file() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("fake app-server did not enter the blocked quota operation");
}

fn response_status(response: &str) -> &str {
    response.lines().next().expect("HTTP response status line")
}

#[test]
fn delete_waits_for_inflight_quota_refresh_before_removing_account() {
    let directory = tempfile::tempdir().expect("account deletion state directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical account deletion state directory");
    let account_root = root.join("state/accounts");
    let auth_path = account_root.join("racing/auth.json.enc");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "account_store_path":account_root,
            "accounts":[{
                "id":"racing",
                "name":"Racing account",
                "prefix":"racing",
                "auth_file":auth_path,
            }],
        }))
        .expect("encode account configuration"),
    )
    .expect("write account configuration");

    let fake_root = tempfile::Builder::new()
        .prefix("emp-delete-quota-fake-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("crate-local fake app-server directory");
    let executable = fake_root.path().join("fake-codex");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
control = pathlib.Path(sys.argv[0]).parent
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id":request["id"],"result":{}}), flush=True)
    elif method == "account/read":
        auth_path = pathlib.Path(os.environ["CODEX_HOME"]) / "auth.json"
        auth = json.loads(auth_path.read_text())
        auth["tokens"]["access_token"] = "quota-race-rotated"
        auth_path.write_text(json.dumps(auth))
        (control / "quota-started").touch()
        deadline = time.monotonic() + 10
        while not (control / "quota-release").exists() and time.monotonic() < deadline:
            time.sleep(0.005)
        assert (control / "quota-release").exists(), "quota fixture was never released"
        print(json.dumps({"id":request["id"],"result":{"account":{"email":"user@example.com","planType":"pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        auth_path = pathlib.Path(os.environ["CODEX_HOME"]) / "auth.json"
        auth = json.loads(auth_path.read_text())
        assert auth["tokens"]["access_token"] == "quota-race-rotated"
        print(json.dumps({"id":request["id"],"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":17,"windowDurationMins":300}}}}), flush=True)
"#,
    )
    .expect("write fake app-server");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("make fake app-server executable");

    let native_auth_path = root.join("codex/auth.json");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        executable.to_str().expect("fake binary path is UTF-8"),
        native_auth_path,
    )
    .expect("start account management server");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"quota-race","account_id":"racing"}}),
        )
        .expect("write encrypted account auth");

    let address = server.local_addr();
    let cookie = server.session_token();
    let quota_cookie = cookie.clone();
    let quota = thread::spawn(move || {
        http_request(
            address,
            "POST",
            "/api/accounts/racing/quota",
            &quota_cookie,
            b"{}",
            None,
        )
    });
    let started = fake_root.path().join("quota-started");
    wait_for_file(&started);

    let (delete_sent, delete_sent_rx) = mpsc::channel();
    let (delete_done, delete_done_rx) = mpsc::channel();
    let delete_cookie = cookie.clone();
    let delete_address = address;
    let delete = thread::spawn(move || {
        let response = http_request(
            delete_address,
            "DELETE",
            "/api/accounts/racing",
            &delete_cookie,
            b"",
            Some(delete_sent),
        );
        delete_done.send(response).expect("send DELETE response");
    });
    delete_sent_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("DELETE request reached the server");
    let early_delete = delete_done_rx.recv_timeout(Duration::from_millis(250)).ok();
    let delete_completed_early = early_delete.is_some();

    std::fs::write(fake_root.path().join("quota-release"), b"release")
        .expect("release fake quota response");
    let quota_response = quota.join().expect("join quota endpoint worker");
    let delete_response = match early_delete {
        Some(response) => response,
        None => delete_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("DELETE completes after quota refresh"),
    };
    delete.join().expect("join DELETE endpoint worker");

    let auth_recreated_after_delete = auth_path.exists();
    let config: Value = serde_json::from_slice(
        &std::fs::read(&config_path).expect("read committed account config"),
    )
    .expect("account config remains valid JSON");
    server
        .shutdown()
        .expect("shutdown account management server");

    assert!(
        !auth_recreated_after_delete,
        "quota refresh must not recreate encrypted auth after account deletion"
    );
    assert!(
        !delete_completed_early,
        "DELETE completed before the in-flight quota endpoint: {delete_response}"
    );
    assert!(
        response_status(&quota_response).starts_with("HTTP/1.1 200 OK"),
        "quota refresh must finish before deletion: {quota_response}"
    );
    assert!(
        response_status(&delete_response).starts_with("HTTP/1.1 200 OK"),
        "account deletion should succeed after quota refresh: {delete_response}"
    );
    assert_eq!(config["accounts"], json!([]));
}

#[test]
fn quota_history_follows_upstream_identity_across_delete_and_reimport() {
    let directory = tempfile::tempdir().expect("quota history identity directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical quota history identity directory");
    let config_path = root.join("config.json");
    let account_root = root.join("state/accounts");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":account_root}))
            .expect("encode account configuration"),
    )
    .expect("write account configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .expect("start quota history account state");

    let import = |id: &str, upstream_id: &str, access_token: &str| {
        import_account_state(
            None,
            &server.state,
            &json!({
                "id": id,
                "name": "Demo",
                "prefix": id,
                "auth_json": {
                    "tokens": {
                        "access_token": access_token,
                        "account_id": upstream_id,
                    }
                },
            }),
        )
        .expect("import account")
    };

    import("demo", "upstream-account-a", "token-before-delete");
    let original_owner = quota_owner_key(&server.state, "demo").expect("original quota owner");
    let quota = json!({
        "rate_limits": {
            "limitId": "codex",
            "primary": {"usedPercent": 23, "windowDurationMins": 300},
        },
    });
    server
        .state
        .backend
        .accounts
        .quota_history
        .append_snapshot(&original_owner, &quota, 2_000_100)
        .expect("record original quota sample");
    delete_account_state(None, &server.state, "demo").expect("delete original account");

    import("demo-restored", "upstream-account-a", "rotated-token");
    assert_eq!(
        quota_owner_key(&server.state, "demo-restored").expect("restored quota owner"),
        original_owner
    );
    let restored = server
        .state
        .backend
        .accounts
        .quota_history
        .query(&original_owner, "1h", 2_000_200)
        .expect("query restored quota history");
    assert_eq!(restored["series"][0]["points"].as_array().unwrap().len(), 1);
    delete_account_state(None, &server.state, "demo-restored").expect("delete restored account");

    import("demo", "upstream-account-b", "other-account-token");
    let other_owner = quota_owner_key(&server.state, "demo").expect("other quota owner");
    assert_ne!(other_owner, original_owner);
    let other_history = server
        .state
        .backend
        .accounts
        .quota_history
        .query(&other_owner, "1h", 2_000_200)
        .expect("query other account history");
    assert_eq!(other_history["series"], json!([]));
    server
        .shutdown()
        .expect("shutdown quota history account state");
}

#[test]
fn legacy_local_key_history_moves_to_the_verified_identity_once_or_is_settled() {
    let directory = tempfile::tempdir().expect("legacy quota history directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical legacy quota history directory");
    let config_path = root.join("config.json");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().unwrap()).expect("native auth directory");
    std::fs::write(
        &native_auth_path,
        serde_json::to_vec(
            &json!({"tokens":{"access_token":"native","account_id":"upstream-native"}}),
        )
        .unwrap(),
    )
    .expect("write native auth");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":root.join("state/accounts")})).unwrap(),
    )
    .expect("write account configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        native_auth_path,
    )
    .expect("start legacy quota history state");
    let import = |id: &str, upstream_id: &str| {
        import_account_state(
            None,
            &server.state,
            &json!({"id": id, "name": id, "prefix": id,
                "auth_json": {"tokens": {"access_token": format!("{id}-token"), "account_id": upstream_id}}}),
        )
        .expect("import account")
    };
    let history = &server.state.backend.accounts.quota_history;
    let sample = |used: u64| json!({"rate_limits":{"limitId":"codex","primary":{"usedPercent":used,"windowDurationMins":300}}});
    let points = |owner: &str| {
        history
            .query(owner, "1h", 2_000_900)
            .expect("query history")["series"]
            .as_array()
            .map_or(0, |series| {
                series
                    .iter()
                    .map(|s| s["points"].as_array().unwrap().len())
                    .sum()
            })
    };

    import("demo", "upstream-demo");
    // Rows written by the previous release under local keys.
    history
        .append_snapshot("demo", &sample(10), 2_000_100)
        .unwrap();
    history
        .append_snapshot("demo", &sample(20), 2_000_400)
        .unwrap();
    history
        .append_snapshot("@native", &sample(30), 2_000_100)
        .unwrap();
    history
        .append_snapshot("ghost", &sample(40), 2_000_100)
        .unwrap();

    crate::services::quota::migrate_legacy_quota_history(&server.state);
    crate::services::quota::migrate_legacy_quota_history(&server.state);
    let owner = quota_owner_key(&server.state, "demo").unwrap();
    let native = quota_owner_key(&server.state, "@native").unwrap();
    assert_eq!(points(&owner), 2);
    assert_eq!(points("demo"), 0);
    // `@native` rows may come from any earlier login: the current one
    // cannot claim them.
    assert_eq!(points(&native), 0);
    assert_eq!(points("@native"), 1);
    // No configured account can vouch for "ghost": its rows stay untouched.
    assert_eq!(points("ghost"), 1);

    // A legacy row written after startup is adopted before the id is freed,
    // so a different account reusing the id never inherits it.
    history
        .append_snapshot("demo", &sample(50), 2_000_700)
        .unwrap();
    delete_account_state(None, &server.state, "demo").expect("delete demo");
    assert_eq!(points(&owner), 3);
    import("demo", "someone-else");
    let other = quota_owner_key(&server.state, "demo").unwrap();
    assert_ne!(other, owner);
    crate::services::quota::migrate_legacy_quota_history(&server.state);
    assert_eq!(points(&other), 0);

    // Reimporting the id with different credentials attributes its legacy
    // rows with the credentials that recorded them first.
    history
        .append_snapshot("demo", &sample(60), 2_000_800)
        .unwrap();
    // A reimport that is rejected leaves the rows alone.
    import("team-a", "upstream-team-a");
    assert!(
        import_account_state(
            None,
            &server.state,
            &json!({"id": "demo", "name": "demo", "prefix": "team-a",
                "auth_json": {"tokens": {"access_token": "demo-token", "account_id": "third-identity"}}}),
        )
        .is_err_and(|error| error.contains("prefix is already in use"))
    );
    assert_eq!(points("demo"), 1);
    assert_eq!(points(&other), 0);
    // Also when only full configuration validation rejects it: the prefix
    // names an existing provider.
    {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        config["providers"] = json!([{"id": "deepseek", "base_url": "https://example.test/v1"}]);
        *config = emp_state::normalize_configuration(Some(&config)).unwrap();
    }
    assert!(
        import_account_state(
            None,
            &server.state,
            &json!({"id": "demo", "name": "demo", "prefix": "deepseek",
                "auth_json": {"tokens": {"access_token": "demo-token", "account_id": "third-identity"}}}),
        )
        .is_err_and(|error| error.contains("conflict"))
    );
    assert_eq!(points("demo"), 1);
    assert_eq!(points(&other), 0);
    import("demo", "third-identity");
    assert_eq!(points(&other), 1);
    assert_eq!(points("demo"), 0);

    // When the recording credentials are unreadable the rows cannot be
    // attributed; deleting the account drops them instead of leaving them
    // for the next account that reuses the id.
    history
        .append_snapshot("demo", &sample(70), 2_000_850)
        .unwrap();
    let owner_auth = emp_state::account_auth_path(
        &server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap()
            .clone(),
        "demo",
        &server.state.backend.configuration.config_path,
    )
    .unwrap();
    std::fs::write(&owner_auth, b"not a vault document").unwrap();
    delete_account_state(None, &server.state, "demo").expect("delete unreadable demo");
    assert_eq!(points("demo"), 0);
    import("demo", "fourth-identity");
    let fourth = quota_owner_key(&server.state, "demo").unwrap();
    crate::services::quota::migrate_legacy_quota_history(&server.state);
    assert_eq!(points(&fourth), 0);
    server
        .shutdown()
        .expect("shutdown legacy quota history state");
}

#[test]
fn account_import_waits_for_the_refresh_lock_and_drops_older_rotations() {
    let directory = tempfile::tempdir().expect("import lock directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical import root");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":root.join("state/accounts")})).unwrap(),
    )
    .expect("write import configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .expect("start import lock state");
    let body = |token: &str| {
        json!({"id": "demo", "name": "demo", "prefix": "demo",
            "auth_json": {"tokens": {"access_token": token, "account_id": "upstream-demo"}}})
    };
    import_account_state(None, &server.state, &body("first")).expect("first import");
    let auth_path = emp_state::account_auth_path(
        &server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap()
            .clone(),
        "demo",
        &server.state.backend.configuration.config_path,
    )
    .unwrap();
    // A rotation of the first credential is still waiting to be saved.
    server
        .state
        .backend
        .accounts
        .pending_rotations
        .lock()
        .unwrap()
        .insert(
            auth_path.to_string_lossy().into_owned(),
            json!({"tokens":{"access_token":"first-rotated","account_id":"upstream-demo"}}),
        );

    let lock = super::quota_refresh_lock(&server.state, "demo").unwrap();
    let guard = lock.lock().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            import_account_state(None, &server.state, &body("second")).expect("second import");
            done_tx.send(()).unwrap();
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "import replaced credentials while a refresh held the account lock"
        );
        drop(guard);
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("import finishes once the lock is free");
    });

    assert_eq!(
        crate::services::quota::flush_pending_rotations(&server.state),
        0
    );
    let stored = server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&auth_path)
        .expect("read imported auth");
    assert_eq!(stored["tokens"]["access_token"], "second");
    server.shutdown().expect("shutdown import lock state");
}

#[test]
fn account_id_is_not_freed_while_its_legacy_history_cannot_be_settled() {
    let directory = tempfile::tempdir().expect("settle failure directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical settle root");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":root.join("state/accounts")})).unwrap(),
    )
    .expect("write settle configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .expect("start settle failure state");
    let body = |upstream: &str| {
        json!({"id": "demo", "name": "demo", "prefix": "demo",
            "auth_json": {"tokens": {"access_token": "token", "account_id": upstream}}})
    };
    import_account_state(None, &server.state, &body("upstream-demo")).expect("import demo");
    // Legacy rows exist, but neither adoption nor deletion can reach them.
    let history_path = server
        .state
        .backend
        .accounts
        .quota_history
        .path()
        .to_path_buf();
    std::fs::write(&history_path, b"not a sqlite database").expect("break quota history");

    assert!(delete_account_state(None, &server.state, "demo").is_err());
    assert!(import_account_state(None, &server.state, &body("someone-else")).is_err());
    let config = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()
        .clone();
    assert!(
        config["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|account| account["id"] == "demo")
    );
    server.shutdown().expect("shutdown settle failure state");
}

#[test]
fn credential_replacement_also_locks_accounts_added_while_it_waited() {
    let directory = tempfile::tempdir().expect("replacement lock directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical replacement root");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":root.join("state/accounts")})).unwrap(),
    )
    .expect("write replacement configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .expect("start replacement lock state");
    let body = |id: &str| {
        json!({"id": id, "name": id, "prefix": id,
            "auth_json": {"tokens": {"access_token": format!("{id}-token"), "account_id": format!("upstream-{id}")}}})
    };
    import_account_state(None, &server.state, &body("a")).expect("import a");
    let lock_a = super::quota_refresh_lock(&server.state, "a").unwrap();
    let guard_a = lock_a.lock().unwrap();
    let (locked_tx, locked_rx) = mpsc::channel();
    thread::scope(|scope| {
        scope.spawn(|| {
            let seen = super::replacing_account_credentials(&server.state, |config| {
                let held = ["a", "b"].map(|id| {
                    super::quota_refresh_lock(&server.state, id)
                        .unwrap()
                        .try_lock()
                        .is_err()
                });
                (config["accounts"].as_array().unwrap().len(), held)
            })
            .expect("replacement runs");
            locked_tx.send(seen).unwrap();
        });
        // The replacement listed only `a` and now waits for its lock; `b`
        // is added in the meantime.
        thread::sleep(Duration::from_millis(100));
        import_account_state(None, &server.state, &body("b")).expect("import b");
        drop(guard_a);
        let (accounts, held) = locked_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("replacement finishes");
        assert_eq!(accounts, 2);
        assert_eq!(held, [true, true], "every snapshot account is locked");
    });
    server.shutdown().expect("shutdown replacement lock state");
}

/// Migration import rewrites the whole configuration. An account imported
/// while it runs must not be overwritten by the migration's older copy: the
/// import waits for the migration and then applies on top of its result.
#[test]
fn account_import_during_credential_replacement_is_not_lost() {
    let directory = tempfile::tempdir().expect("replacement race directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical race root");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":root.join("state/accounts")})).unwrap(),
    )
    .expect("write race configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .expect("start race state");
    let body = |id: &str| {
        json!({"id": id, "name": id, "prefix": id,
            "auth_json": {"tokens": {"access_token": format!("{id}-token"), "account_id": format!("upstream-{id}")}}})
    };
    import_account_state(None, &server.state, &body("a")).expect("import a");
    let (imported_tx, imported_rx) = mpsc::channel();
    thread::scope(|scope| {
        super::replacing_account_credentials(&server.state, |config| {
            scope.spawn(|| {
                imported_tx
                    .send(import_account_state(None, &server.state, &body("b")).is_ok())
                    .unwrap();
            });
            thread::sleep(Duration::from_millis(200));
            assert!(
                imported_rx.try_recv().is_err(),
                "an import completed while the replacement held the configuration"
            );
            // The migration's own change to the configuration it was given.
            let mut updated = config.clone();
            updated["accounts"][0]["name"] = json!("renamed-by-migration");
            config.commit(&updated).expect("commit migration change");
        })
        .expect("replacement runs");
    });
    assert!(
        imported_rx.recv().unwrap(),
        "import b succeeds after the replacement"
    );
    let config = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()
        .clone();
    let accounts = config["accounts"].as_array().unwrap();
    let names = accounts
        .iter()
        .map(|account| account["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, ["renamed-by-migration", "b"]);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(saved["accounts"].as_array().unwrap().len(), 2);
    server.shutdown().expect("shutdown race state");
}

/// Settling an id's legacy history is part of the reimport's validated
/// commit: while it runs (paused here by an exclusive SQLite lock), a
/// concurrent import taking the same prefix must wait and then be rejected,
/// rather than invalidate the reimport after its history was settled.
#[test]
fn a_concurrent_prefix_change_cannot_reject_a_reimport_after_its_history_was_settled() {
    let directory = tempfile::tempdir().expect("settle race directory");
    let root = directory
        .path()
        .canonicalize()
        .expect("canonical settle race root");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({"account_store_path":root.join("state/accounts")})).unwrap(),
    )
    .expect("write settle race configuration");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "/bin/false",
        root.join("codex/auth.json"),
    )
    .expect("start settle race state");
    let body = |id: &str, prefix: &str, upstream: &str| {
        json!({"id": id, "name": id, "prefix": prefix,
            "auth_json": {"tokens": {"access_token": format!("{id}-token"), "account_id": upstream}}})
    };
    import_account_state(None, &server.state, &body("demo", "demo", "upstream-demo"))
        .expect("import demo");
    let history = &server.state.backend.accounts.quota_history;
    history
        .append_snapshot(
            "demo",
            &json!({"rate_limits":{"limitId":"codex","primary":{"usedPercent":10,"windowDurationMins":300}}}),
            2_000_100,
        )
        .unwrap();
    let blocker = rusqlite::Connection::open(root.join("state/quota_history.sqlite3"))
        .expect("open quota history");
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("pause history writes");
    let configuration = &server.state.backend.configuration.test_config();
    let (reimport, rival) = thread::scope(|scope| {
        let reimport = scope.spawn(|| {
            import_account_state(
                None,
                &server.state,
                &body("demo", "team-b", "upstream-demo-2"),
            )
        });
        // Wait until the reimport holds the configuration (it is then
        // settling, blocked on the history database).
        let deadline = Instant::now() + Duration::from_secs(2);
        while configuration.try_lock().is_ok() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let rival = scope.spawn(|| {
            import_account_state(
                None,
                &server.state,
                &body("team-a", "team-b", "upstream-team-a"),
            )
        });
        thread::sleep(Duration::from_millis(200));
        blocker
            .execute_batch("COMMIT")
            .expect("resume history writes");
        (reimport.join().unwrap(), rival.join().unwrap())
    });
    reimport.expect("the settled reimport commits");
    assert!(
        rival.is_err_and(|error| error.contains("prefix is already in use")),
        "the rival import waits for the reimport and is rejected"
    );
    let config = configuration.lock().unwrap().clone();
    assert_eq!(config["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(config["accounts"][0]["prefix"], "team-b");
    server.shutdown().expect("shutdown settle race state");
}
