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
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
        print(json.dumps({"id":request["id"],"result":{"account":{"email":"xian@example.com","planType":"pro"}}}), flush=True)
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
    let cookie = server
        .session_cookie()
        .split(';')
        .next()
        .expect("session cookie pair")
        .to_owned();
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
