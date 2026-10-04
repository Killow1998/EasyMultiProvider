#![cfg(unix)]

use super::{canonical_root, post, session_header};
use crate::lifecycle::ServerHandle;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::fs::PermissionsExt;

fn status_and_body(response: String) -> (String, Value) {
    let (head, body) = response.split_once("\r\n\r\n").expect("HTTP response");
    (
        head.lines().next().unwrap_or_default().to_owned(),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// Codex may invalidate the stored refresh token as soon as it rotates the
/// credential. When saving the rotated credential fails, it must survive in
/// memory, drive the next quota check, and reach disk once saving works.
#[test]
fn rotated_credentials_survive_a_failed_save_and_are_persisted_later() {
    let directory = tempfile::tempdir().expect("rotation temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state/accounts");
    let auth_path = account_root.join("rotating/auth.json.enc");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "account_store_path": account_root,
            "accounts": [{"id":"rotating","name":"rotating","prefix":"rotating","auth_file":auth_path}],
        }))
        .expect("encode rotation config"),
    )
    .expect("write rotation config");
    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-rotation-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("crate-local fake Codex directory");
    let executable = executable_root.path().join("fake-codex");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
home = pathlib.Path(os.environ["CODEX_HOME"])
calls = pathlib.Path(sys.argv[0]).with_suffix(".calls")
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id":request["id"],"result":{}}), flush=True)
    elif method == "account/read":
        auth_path = home / "auth.json"
        auth = json.loads(auth_path.read_text())
        started = auth["tokens"]["access_token"]
        with calls.open("a", encoding="utf-8") as log:
            log.write(started + "\n")
        if started == "original":
            auth["tokens"]["access_token"] = "rotated"
            auth["tokens"]["refresh_token"] = "rotated-refresh"
            auth_path.write_text(json.dumps(auth))
        elif started != "rotated":
            raise AssertionError("stale credential reused: " + started)
        print(json.dumps({"id":request["id"],"result":{"account":{"email":"user@example.com","planType":"pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        print(json.dumps({"id":request["id"],"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":9,"windowDurationMins":300}}}}), flush=True)
"#,
    )
    .expect("write fake Codex app-server");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("make fake Codex executable");

    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        executable.to_str().expect("fake Codex path is UTF-8"),
        root.join("codex/auth.json"),
    )
    .expect("start quota service");
    let vault = &server.state.backend.configuration.vault;
    vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"original","refresh_token":"original-refresh","account_id":"upstream"}}),
        )
        .expect("write original auth");
    let session = session_header(&server);
    let fail_saves = |failing: bool| {
        let mut failing_paths = crate::services::quota::FAIL_ROTATION_SAVES_TO
            .lock()
            .unwrap();
        let path = auth_path.to_str().unwrap().to_owned();
        if failing {
            failing_paths.insert(path);
        } else {
            failing_paths.remove(&path);
        }
    };

    // Saving fails: the rotated credential is reported and kept in memory.
    fail_saves(true);
    let (status, body) = status_and_body(post(
        &server,
        "/api/accounts/rotating/quota",
        b"{}",
        &[&session],
    ));
    assert_ne!(status, "HTTP/1.1 200 OK", "{body}");
    assert_eq!(
        body["error"]["code"], "quota_credentials_save_failed",
        "{body}"
    );
    let pending = server
        .state
        .backend
        .accounts
        .pending_rotations
        .lock()
        .unwrap()
        .get(auth_path.to_str().unwrap())
        .cloned()
        .expect("rotated credential kept in memory");
    assert_eq!(pending["tokens"]["access_token"], "rotated");

    // Still unwritable: the next check uses the rotated credential, not the
    // stale stored one, and keeps it pending.
    let (status, body) = status_and_body(post(
        &server,
        "/api/accounts/rotating/quota",
        b"{}",
        &[&session],
    ));
    assert_eq!(status, "HTTP/1.1 200 OK", "{body}");
    assert!(
        server
            .state
            .backend
            .accounts
            .pending_rotations
            .lock()
            .unwrap()
            .contains_key(auth_path.to_str().unwrap())
    );

    // Nothing flushes while saving still fails.
    assert_eq!(
        crate::services::quota::flush_pending_rotations(&server.state),
        1
    );

    // A failed re-import or removal must roll back its credential files and
    // retain the rotated token. Clearing pending state before the config
    // commit would discard the only usable credential.
    let original_file = std::fs::read(&auth_path).unwrap();
    let original_config = server.state.backend.configuration.snapshot().unwrap();
    let saved_config = root.join("config.saved.json");
    std::fs::rename(&config_path, &saved_config).unwrap();
    std::fs::create_dir(&config_path).unwrap();
    assert!(
        crate::services::accounts::import_account_state(
            &server.state,
            &json!({"id":"rotating", "prefix":"rotating", "auth_json":{
                "tokens":{"access_token":"replacement", "account_id":"upstream"}
            }})
        )
        .is_err()
    );
    assert!(crate::services::accounts::delete_account_state(&server.state, "rotating").is_err());
    assert_eq!(std::fs::read(&auth_path).unwrap(), original_file);
    assert_eq!(
        server.state.backend.configuration.snapshot().unwrap(),
        original_config
    );
    assert_eq!(
        server
            .state
            .backend
            .accounts
            .pending_rotations
            .lock()
            .unwrap()
            .get(auth_path.to_str().unwrap())
            .unwrap()["tokens"]["access_token"],
        "rotated"
    );
    std::fs::remove_dir(&config_path).unwrap();
    std::fs::rename(&saved_config, &config_path).unwrap();

    // Writable again: the pending credential reaches disk and is cleared.
    fail_saves(false);
    let (status, body) = status_and_body(post(
        &server,
        "/api/accounts/rotating/quota",
        b"{}",
        &[&session],
    ));
    assert_eq!(status, "HTTP/1.1 200 OK", "{body}");
    let stored = vault
        .read_encrypted_json(&auth_path)
        .expect("read persisted rotated auth");
    assert_eq!(stored["tokens"]["access_token"], "rotated");
    assert_eq!(stored["tokens"]["refresh_token"], "rotated-refresh");
    assert!(
        server
            .state
            .backend
            .accounts
            .pending_rotations
            .lock()
            .unwrap()
            .is_empty()
    );

    let calls = std::fs::read_to_string(executable.with_extension("calls"))
        .expect("read fake app-server calls");
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["original", "rotated", "rotated"]
    );
    server.shutdown().expect("shutdown quota service");
}

/// A rotated credential whose save failed must reach disk at shutdown even
/// if no further quota check runs, and a pending credential for an account
/// that no longer exists is dropped rather than written.
#[test]
fn shutdown_flushes_pending_rotations_of_configured_accounts_only() {
    let directory = tempfile::tempdir().expect("flush temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state/accounts");
    let auth_path = account_root.join("kept/auth.json.enc");
    let gone_path = account_root.join("gone/auth.json.enc");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "account_store_path": account_root,
            "accounts": [{"id":"kept","name":"kept","prefix":"kept","auth_file":auth_path}],
        }))
        .expect("encode flush config"),
    )
    .expect("write flush config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "codex-not-needed",
        root.join("codex/auth.json"),
    )
    .expect("start quota service");
    let state = std::sync::Arc::clone(&server.state);
    let vault = &state.backend.configuration.vault;
    vault
        .write_encrypted_json(&auth_path, &json!({"tokens":{"access_token":"stale"}}))
        .expect("write stale auth");
    {
        let mut pending = server
            .state
            .backend
            .accounts
            .pending_rotations
            .lock()
            .unwrap();
        pending.insert(
            auth_path.to_str().unwrap().to_owned(),
            json!({"tokens":{"access_token":"rotated"}}),
        );
        pending.insert(
            gone_path.to_str().unwrap().to_owned(),
            json!({"tokens":{"access_token":"orphan"}}),
        );
    }
    server.shutdown().expect("shutdown quota service");
    let stored = vault
        .read_encrypted_json(&auth_path)
        .expect("read flushed auth");
    assert_eq!(stored["tokens"]["access_token"], "rotated");
    assert!(!gone_path.exists());
}

/// Shutdown that cannot save a rotated credential is not a clean exit: the
/// only valid credential is lost, so the caller must see an error.
#[test]
fn shutdown_reports_rotated_credentials_it_could_not_save() {
    let directory = tempfile::tempdir().expect("unsaved temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state/accounts");
    let auth_path = account_root.join("unsaved/auth.json.enc");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "account_store_path": account_root,
            "accounts": [{"id":"unsaved","name":"unsaved","prefix":"unsaved","auth_file":auth_path}],
        }))
        .expect("encode unsaved config"),
    )
    .expect("write unsaved config");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        "codex-not-needed",
        root.join("codex/auth.json"),
    )
    .expect("start quota service");
    server
        .state
        .backend
        .accounts
        .pending_rotations
        .lock()
        .unwrap()
        .insert(
            auth_path.to_str().unwrap().to_owned(),
            json!({"tokens":{"access_token":"rotated"}}),
        );
    let failing_path = auth_path.to_str().unwrap().to_owned();
    crate::services::quota::FAIL_ROTATION_SAVES_TO
        .lock()
        .unwrap()
        .insert(failing_path.clone());
    let result = server.shutdown();
    crate::services::quota::FAIL_ROTATION_SAVES_TO
        .lock()
        .unwrap()
        .remove(&failing_path);
    assert!(
        matches!(result, Err(crate::error::AppError::CredentialsUnsaved(1))),
        "{result:?}"
    );
}

/// Request threads are not joined at shutdown. A quota check whose Codex
/// already rotated the credential, but which has not saved it (or left it
/// pending) yet, must still finish before the final save and exit.
#[test]
fn shutdown_waits_for_a_quota_check_that_is_rotating_a_credential() {
    let directory = tempfile::tempdir().expect("drain temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state/accounts");
    let auth_path = account_root.join("draining/auth.json.enc");
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "account_store_path": account_root,
            "accounts": [{"id":"draining","name":"draining","prefix":"draining","auth_file":auth_path}],
        }))
        .expect("encode drain config"),
    )
    .expect("write drain config");
    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-draining-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("crate-local fake Codex directory");
    let executable = executable_root.path().join("fake-codex");
    // Rotates the credential, announces it, then holds its reply until
    // released, like a slow upstream after the refresh.
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
home = pathlib.Path(os.environ["CODEX_HOME"])
me = pathlib.Path(sys.argv[0])
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id":request["id"],"result":{}}), flush=True)
    elif method == "account/read":
        auth_path = home / "auth.json"
        auth = json.loads(auth_path.read_text())
        if auth["tokens"]["access_token"] == "original":
            auth["tokens"]["access_token"] = "rotated"
            auth_path.write_text(json.dumps(auth))
            me.with_suffix(".rotated").touch()
            while not me.with_suffix(".release").exists():
                time.sleep(0.02)
        print(json.dumps({"id":request["id"],"result":{"account":{"email":"user@example.com","planType":"pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        print(json.dumps({"id":request["id"],"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":9,"windowDurationMins":300}}}}), flush=True)
"#,
    )
    .expect("write fake Codex app-server");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("make fake Codex executable");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config_path,
        executable.to_str().expect("fake Codex path is UTF-8"),
        root.join("codex/auth.json"),
    )
    .expect("start quota service");
    let state = std::sync::Arc::clone(&server.state);
    state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_path,
            &json!({"tokens":{"access_token":"original","refresh_token":"original-refresh","account_id":"upstream"}}),
        )
        .expect("write original auth");
    let session = session_header(&server);
    // Send the check without waiting for its reply, which the held Codex
    // delays past the shutdown call.
    let mut client =
        std::net::TcpStream::connect(server.local_addr()).expect("connect quota check");
    std::io::Write::write_all(
        &mut client,
        format!(
            "POST /api/accounts/draining/quota HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n{session}\r\nConnection: close\r\n\r\n{{}}",
            server.local_addr().port()
        )
        .as_bytes(),
    )
    .expect("send quota check");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !executable.with_extension("rotated").exists() {
        assert!(std::time::Instant::now() < deadline, "Codex never rotated");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let release = executable.with_extension("release");
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::fs::write(release, b"").expect("release fake Codex");
    });
    server
        .shutdown()
        .expect("shutdown after the rotation is saved");
    let stored = state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&auth_path)
        .expect("read saved auth");
    assert_eq!(stored["tokens"]["access_token"], "rotated");
    releaser.join().expect("releaser");
    drop(client);
}
