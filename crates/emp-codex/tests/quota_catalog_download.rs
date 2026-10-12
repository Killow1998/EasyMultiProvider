#![cfg(unix)]

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_codex::quota::{
    consume_native_quota_reset, read_native_login_quota, run_quota_query_persisting,
};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[test]
#[ignore = "requires installed Codex and isolated loopback; synthetic auth only"]
fn installed_codex_quota_preserves_native_credentials_and_persists_imported_rotation() {
    let binary = std::env::var("EMP_TEST_CODEX_CLI")
        .expect("set EMP_TEST_CODEX_CLI to the installed Codex executable");
    let root = tempfile::Builder::new()
        .prefix("emp-quota-catalog-")
        .tempdir()
        .unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    std::fs::write(root.path().join("ca.pem"), certificate.cert.pem()).unwrap();
    let tls = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.cert.der().clone()],
                PrivatePkcs8KeyDer::from(certificate.signing_key.serialize_der()).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("https://{}/backend-api", listener.local_addr().unwrap());
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server = std::thread::spawn(move || {
        let mut paths = Vec::new();
        while !stopping.load(Ordering::Acquire) {
            let (stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("fixture accept: {error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut stream = StreamOwned::new(ServerConnection::new(tls.clone()).unwrap(), stream);
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap().to_owned();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            drop(reader);
            let body = if path.ends_with("/oauth/token") {
                let claims = json!({"email":"quota@example.test","exp":2100000000,
                    "https://api.openai.com/auth":{"chatgpt_plan_type":"plus", "chatgpt_user_id":"fixture-user","chatgpt_account_id":"fixture-account"}});
                let token = format!(
                    "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{}.c2ln",
                    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
                );
                serde_json::to_vec(&json!({"id_token":token,"access_token":"rotated-fixture-access","refresh_token":"rotated-fixture-refresh"})).unwrap()
            } else if path.ends_with("/rate-limit-reset-credits/consume") {
                serde_json::to_vec(&json!({"code":"reset","windows_reset":1})).unwrap()
            } else if path.contains("/models") {
                include_bytes!("../src/quota/process/quota-models.json").to_vec()
            } else if path.ends_with("/accounts/check") {
                serde_json::to_vec(&json!({"account_ordering":["fixture-account"],"default_account_id":"fixture-account","accounts":[{
                    "id":"fixture-account", "workspace_backend_origin":origin,
                    "account_routing_override":"NO_CONSTRAINT", "plan_type":"plus",
                    "name":"Fixture", "structure":"personal", "profile_picture_url":null
                }]})).unwrap()
            } else {
                serde_json::to_vec(&json!({
                    "plan_type":"plus", "rate_limit":{
                        "allowed":true, "limit_reached":false,
                        "primary_window":{"used_percent":7,"limit_window_seconds":18000,
                            "reset_after_seconds":60,"reset_at":2000000000}
                    }
                }))
                .unwrap()
            };
            paths.push(path);
            // Leave enough time for unwanted background model requests to start.
            std::thread::sleep(Duration::from_millis(500));
            let _ = write!(
                stream,
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
        paths
    });
    let launcher = root.path().join("codex");
    // Only the test launcher changes the backend URL. The production helper
    // still writes its own private auth/config/catalog and controls its lifetime.
    std::fs::write(
        &launcher,
        format!(
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys
home = pathlib.Path(os.environ['CODEX_HOME'])
config = home / 'config.toml'
config.write_text('chatgpt_base_url = ' + json.dumps({base}) + '\nopenai_base_url = ' + json.dumps({base} + '/codex') + '\n' + config.read_text())
for key in ['HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy']:
    os.environ[key] = 'http://127.0.0.1:1'
os.environ['NO_PROXY'] = os.environ['no_proxy'] = '127.0.0.1,localhost'
os.environ['CODEX_REFRESH_TOKEN_URL_OVERRIDE'] = {base} + '/oauth/token'
os.environ['CODEX_CA_CERTIFICATE'] = str(pathlib.Path(__file__).parent / 'ca.pem')
os.execv({binary}, [{binary}, *sys.argv[1:]])
"#,
            base = serde_json::to_string(&base).unwrap(),
            binary = serde_json::to_string(&binary).unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let claims = json!({"email":"quota@example.test", "exp":2100000000,
        "https://api.openai.com/auth":{"chatgpt_plan_type":"plus",
            "chatgpt_user_id":"fixture-user","chatgpt_account_id":"fixture-account"}});
    let token = format!(
        "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.{}.c2ln",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let auth = json!({"auth_mode":"chatgpt","tokens":{"id_token":token,"access_token":"fixture-only",
        "refresh_token":"fixture-only","account_id":"fixture-account"},
        "last_refresh":"2020-01-01T00:00:00Z"});
    let native_path = root.path().join("native-auth.json");
    let native_bytes = serde_json::to_vec(&auth).unwrap();
    std::fs::write(&native_path, &native_bytes).unwrap();
    let results = (0..2)
        .map(|_| {
            read_native_login_quota(
                &native_path,
                launcher.to_str().unwrap(),
                Duration::from_secs(15),
            )
        })
        .collect::<Vec<_>>();
    let reset = consume_native_quota_reset(
        &native_path,
        launcher.to_str().unwrap(),
        Duration::from_secs(15),
        "12345678-1234-4123-8123-123456789abc",
        None,
    );
    assert_eq!(std::fs::read(&native_path).unwrap(), native_bytes);
    let mut persisted = None;
    let imported = run_quota_query_persisting(
        &auth,
        launcher.to_str().unwrap(),
        Duration::from_secs(15),
        false,
        |rotated| {
            persisted = Some(rotated.clone());
            Ok(())
        },
    );
    stop.store(true, Ordering::Release);
    let paths = server.join().unwrap();
    println!("captured HTTP requests: {paths:?}");
    for result in results {
        assert!(
            result.is_ok(),
            "{result:?}; captured HTTP requests: {paths:?}"
        );
        let quota = result.expect("actual Codex quota read");
        assert_eq!(quota["rate_limits"]["primary"]["usedPercent"], 7);
    }
    assert_eq!(
        paths.iter().filter(|path| path.ends_with("/usage")).count(),
        3,
        "quota requests: {paths:?}"
    );
    assert_eq!(reset.expect("native reset"), "reset");
    assert!(imported.is_ok(), "{imported:?}");
    assert_eq!(
        persisted.expect("imported rotation persisted")["tokens"]["refresh_token"],
        "rotated-fixture-refresh"
    );
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.ends_with("/oauth/token"))
            .count(),
        1,
        "only the managed imported account may refresh: {paths:?}"
    );
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.ends_with("/consume"))
            .count(),
        1
    );
    assert!(
        paths.iter().all(|path| path == "/backend-api/wham/usage"
            || path.ends_with("/accounts/check")
            || path.ends_with("/rate-limit-reset-credits")
            || path.ends_with("/rate-limit-reset-credits/consume")
            || path.ends_with("/oauth/token")),
        "unexpected requests: {paths:?}"
    );
}
