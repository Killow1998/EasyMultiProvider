#![cfg(unix)]

use super::{canonical_root, post, request, session_cookie_header};
use crate::lifecycle::ServerHandle;
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr};

#[cfg(unix)]
#[test]
fn workspace_routing_error_retries_imported_once_and_never_refreshes_native() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("quota workspace temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state/accounts");
    let native_auth_path = root.join("codex/auth.json");
    std::fs::create_dir_all(native_auth_path.parent().unwrap()).expect("native auth directory");
    let native_auth = json!({"tokens":{"access_token":"native-workspace","account_id":"native"}});
    let native_auth_bytes = serde_json::to_vec(&native_auth).expect("native auth JSON");
    std::fs::write(&native_auth_path, &native_auth_bytes).expect("write native auth");
    let accounts = [
        ("retry-ok", "imported-success"),
        ("retry-fails", "imported-fail"),
    ];
    let config_accounts = accounts
        .iter()
        .map(|(id, _)| {
            json!({
                "id":id,
                "name":id,
                "prefix":id,
                "auth_file":account_root.join(id).join("auth.json.enc"),
            })
        })
        .collect::<Vec<_>>();
    let config_path = root.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "account_store_path":account_root,
            "accounts":config_accounts,
        }))
        .expect("encode quota config"),
    )
    .expect("write quota config");
    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-quota-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("crate-local fake Codex directory");
    let executable = executable_root.path().join("fake-codex");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys
home = pathlib.Path(os.environ["CODEX_HOME"])
calls = pathlib.Path(sys.argv[0]).with_suffix(".calls")
started = ""
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id":request["id"],"result":{}}), flush=True)
    elif method == "account/read":
        auth_path = home / "auth.json"
        auth = json.loads(auth_path.read_text())
        started = auth["tokens"]["access_token"]
        refresh = request["params"]["refreshToken"]
        with calls.open("a", encoding="utf-8") as log:
            log.write(f"{started}:{str(refresh).lower()}\n")
        if started == "imported-success" and not refresh:
            auth["tokens"]["access_token"] = "imported-success-rotated"
            auth_path.write_text(json.dumps(auth))
            print(json.dumps({"id":request["id"],"error":{"code":-32603,"message":"workspace routing discovery timed out"}}), flush=True)
        elif started in ("native-workspace", "imported-fail"):
            print(json.dumps({"id":request["id"],"error":{"code":-32603,"message":"workspace routing discovery failed"}}), flush=True)
        elif started == "imported-success-rotated" and refresh:
            print(json.dumps({"id":request["id"],"result":{"account":{"email":"xian@example.com","planType":"pro"}}}), flush=True)
        else:
            raise AssertionError(f"unexpected account/read state: {started} {refresh}")
    elif method == "account/rateLimits/read":
        assert started == "imported-success-rotated"
        print(json.dumps({"id":request["id"],"result":{"rateLimits":{"limitId":"codex","primary":{"usedPercent":17,"windowDurationMins":300}}}}), flush=True)
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
        native_auth_path.clone(),
    )
    .expect("start quota service");
    for (id, token) in accounts {
        let auth_path = account_root.join(id).join("auth.json.enc");
        server
            .state
            .backend
            .configuration
            .vault
            .write_encrypted_json(
                &auth_path,
                &json!({"tokens":{"access_token":token,"refresh_token":"refresh","account_id":id}}),
            )
            .expect("write imported auth");
    }
    let cookie = session_cookie_header(&server);

    let native = post(&server, "/api/accounts/%40native/quota", b"{}", &[&cookie]);
    assert!(
        native.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{native}"
    );
    let native_body: Value = serde_json::from_str(
        native
            .split_once("\r\n\r\n")
            .expect("native error separator")
            .1,
    )
    .expect("native quota error JSON");
    assert_eq!(
        native_body["error"]["code"], "quota_transport_error",
        "{native}"
    );
    assert_eq!(
        native_body["error"]["message"],
        "Codex could not reach ChatGPT workspace routing; check DNS, VPN/TUN, proxy, and network connectivity"
    );
    assert!(!native.contains("workspace routing discovery failed"));
    assert_eq!(std::fs::read(&native_auth_path).unwrap(), native_auth_bytes);

    let successful = post(&server, "/api/accounts/retry-ok/quota", b"{}", &[&cookie]);
    assert!(
        successful.starts_with("HTTP/1.1 200 OK\r\n"),
        "{successful}"
    );
    let successful: Value = serde_json::from_str(
        successful
            .split_once("\r\n\r\n")
            .expect("imported success separator")
            .1,
    )
    .expect("imported quota response");
    assert_eq!(successful["account"]["credential_status"], "valid");
    assert_eq!(
        successful["account"]["quota"]["rate_limits"]["primary"]["usedPercent"],
        17
    );
    let rotated = server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&account_root.join("retry-ok/auth.json.enc"))
        .expect("read persisted rotated auth");
    assert_eq!(
        rotated["tokens"]["access_token"],
        "imported-success-rotated"
    );

    let failed = post(
        &server,
        "/api/accounts/retry-fails/quota",
        b"{}",
        &[&cookie],
    );
    assert!(
        failed.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{failed}"
    );
    let failed: Value = serde_json::from_str(
        failed
            .split_once("\r\n\r\n")
            .expect("retry error separator")
            .1,
    )
    .expect("retry error JSON");
    assert_eq!(failed["error"]["code"], "quota_transport_error");
    assert!(!failed.to_string().contains("workspace routing discovery"));
    let accounts_response = request(&server, "/api/accounts", &[&cookie]);
    let accounts_snapshot: Value = serde_json::from_str(
        accounts_response
            .split_once("\r\n\r\n")
            .expect("accounts response separator")
            .1,
    )
    .expect("accounts response JSON");
    assert_ne!(
        accounts_snapshot["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|account| account["id"] == "retry-fails")
            .unwrap()["credential_status"],
        "invalid",
        "transport failures must not invalidate imported credentials"
    );

    let calls = std::fs::read_to_string(executable.with_extension("calls"))
        .expect("read fake app-server account/read attempts");
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [
            "native-workspace:false",
            "imported-success:false",
            "imported-success-rotated:true",
            "imported-fail:false",
            "imported-fail:true",
        ]
    );
    server.shutdown().expect("shutdown quota service");
}
