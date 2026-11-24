use super::*;
use std::{io::ErrorKind, os::unix::fs::PermissionsExt, path::Path};
use tokio::{io::AsyncBufReadExt, net::TcpListener as AsyncListener};

const IMPORT: &str = "/api/accounts/import";
const CONFIG: &str = "/api/config";
const ACCOUNT: &str = "/api/accounts/%64emo";
const MODELS: &str = "/api/accounts/%64emo/models/refresh";
const QUOTA: &str = "/api/accounts/%64emo/quota";
const RESET: &str = "/api/accounts/%64emo/quota-reset";
const ACCOUNT_READ: &str = "/api/accounts/demo/models";
const AUTH: &str = "accounts/demo/auth.json.enc";
const IMPORT_BODY: &[u8] = br#"{"id":"demo","prefix":"demo","name":"Initial","auth_json":{"tokens":{"access_token":"fixture-token","account_id":"fixture-owner"}},"other":true}"#;
const RESET_BODY: &[u8] = br#"{"idempotency_key":"12345678-1234-4123-8123-123456789ABC","credit_id":"fixture-credit","other":true}"#;

fn fake_codex(path: &Path) {
    std::fs::write(
        path,
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then
    echo 'codex-cli 0.159.2'
    exit 0
fi
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*) echo '{"id":1,"result":{}}' ;;
        *'"method":"account/read"'*) echo '{"id":2,"result":{"account":{"planType":"pro"}}}' ;;
        *'"method":"account/rateLimits/read"'*) echo '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":7,"windowDurationMins":300,"resetsAt":123}}}}' ;;
        *'"method":"account/rateLimitResetCredit/consume"'*)
            case "$request" in
                *'"creditId":"fixture-credit"'*'"idempotencyKey":"12345678-1234-4123-8123-123456789abc"'*) echo '{"id":3,"result":{"outcome":"reset"}}' ;;
                *) exit 1 ;;
            esac ;;
    esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn fixture(base: SocketAddr, executable: &Path) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let config = root.join("config.json");
    std::fs::write(
        &config,
        json!({
            "codex_base_url": format!("http://{base}/v1"),
            "native_catalog_path": root.join("native.json"),
            "account_store_path": root.join("accounts")
        })
        .to_string(),
    )
    .unwrap();
    let legacy = LegacyServer::start_with_config_options_and_http_client(
        LOOPBACK.ip(),
        0,
        &config,
        executable.to_str().unwrap(),
        root.join("codex/auth.json"),
        emp_transport::HttpClient::new(emp_transport::HttpClientPolicy::default()).unwrap(),
    )
    .unwrap();
    Fixture {
        directory,
        legacy,
        upstream: Arc::default(),
    }
}

pub(super) struct Pair<'a> {
    pub(super) old: &'a Fixture,
    pub(super) new: &'a Fixture,
    pub(super) candidate: SocketAddr,
}

impl Pair<'_> {
    pub(super) async fn parity(
        &self,
        make: impl Fn(u16, &str) -> Vec<u8>,
        status: u16,
        normalize: impl Fn(Vec<u8>) -> Vec<u8>,
    ) -> Value {
        let old = make(self.old.port(), &self.old.session());
        let new = make(self.new.port(), &self.new.session());
        let reply = assert_wire_parity_with(
            [(self.old.legacy.local_addr(), &old), (self.candidate, &new)],
            false,
            normalize,
        )
        .await;
        assert_status(&reply, status);
        json_body(&reply)
    }

    pub(super) async fn post(&self, path: &str, body: &[u8], status: u16) -> Value {
        self.parity(
            |port, session| json_request(port, path, &[session], body),
            status,
            |reply| reply,
        )
        .await
    }

    pub(super) async fn read(&self, path: &str) -> Value {
        self.parity(
            |port, session| request(port, "GET", path, &[session], b""),
            200,
            |reply| reply,
        )
        .await
    }

    fn saved(&self) -> Value {
        let read = |fixture: &Fixture| {
            let config = std::fs::read(fixture.directory.path().join("config.json")).unwrap();
            serde_json::from_slice::<Value>(&config).unwrap()
        };
        let mut old = read(self.old)["accounts"].clone();
        let mut new = read(self.new)["accounts"].clone();
        for accounts in [&mut old, &mut new] {
            for account in accounts.as_array_mut().unwrap() {
                assert!(
                    account
                        .as_object_mut()
                        .unwrap()
                        .remove("auth_file")
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .ends_with(AUTH)
                );
                if let Some(quota) = account["quota"].as_object_mut() {
                    assert!(quota.remove("updated_at").unwrap().as_u64().is_some());
                }
            }
        }
        assert_eq!(old, new);
        old
    }
}

async fn wait_for_model_fetch(upstream: &AsyncListener) -> TcpStream {
    expect_upstream_get(
        upstream,
        "/v1/models?client_version=0.159.2",
        "fixture-token",
    )
    .await
}

pub(super) async fn expect_upstream_get(
    upstream: &AsyncListener,
    path: &str,
    token: &str,
) -> TcpStream {
    let expected_request = format!("GET {path} HTTP/1.1\r\n");
    let authorization = format!("Authorization: Bearer {token}\r\n");
    timeout(Duration::from_secs(5), async {
        let (stream, _) = upstream.accept().await.unwrap();
        let mut stream = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert_eq!(line, expected_request);
        let mut authorized = false;
        loop {
            line.clear();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            authorized |= line.eq_ignore_ascii_case(&authorization);
        }
        assert!(authorized);
        stream.into_inner()
    })
    .await
    .expect("subscription catalog request")
}

async fn send_models(mut stream: TcpStream, slug: &str) {
    let body = json!({"models":[{"slug":slug,"display_name":"Fixture model","supported_in_api":true,"context_window":16000}]}).to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
}

pub(super) fn normalize_body(reply: Vec<u8>, transform: impl FnOnce(&str) -> String) -> Vec<u8> {
    let reply = String::from_utf8(reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap();
    let length = format!("Content-Length: {}\r\n", body.len());
    assert!(head.contains(&length));
    let body = transform(body);
    let head = head.replace(&length, &format!("Content-Length: {}\r\n", body.len()));
    format!("{head}\r\n\r\n{body}").into_bytes()
}

fn normalize_quota_time(reply: Vec<u8>) -> Vec<u8> {
    let body = json_body(&reply);
    let time = body["account"]["quota"]["updated_at"].as_u64().unwrap();
    assert!(time > 0);
    normalize_body(reply, |body| {
        body.replace(&format!("\"updated_at\":{time}"), "\"updated_at\":0")
    })
}

#[test]
fn login_account_reads_and_delete_match_legacy_wire() {
    let old = fixture(LOOPBACK, Path::new("missing-test-codex"));
    fixture(LOOPBACK, Path::new("missing-test-codex")).run_candidate(async |new, candidate| {
        let pair = Pair { old: &old, new, candidate };
        pair.post(
            IMPORT,
            br#"{"id":"login","prefix":"login","auth_json":{"tokens":{"access_token":"fixture-token","account_id":"fixture-owner"}}}"#,
            200,
        )
        .await;

        let models = "/api/accounts/login/models";
        let history = "/api/accounts/login/quota-history?start=1&end=100";
        let account = "/api/accounts/login/";
        for (method, path) in [("GET", models), ("GET", history), ("DELETE", account)] {
            pair.parity(
                |port, _| request(port, method, path, &[], b""),
                401,
                |reply| reply,
            )
            .await;
        }
        assert_eq!(pair.read(models).await["models"], json!([]));
        assert_eq!(pair.read(history).await["account_id"], "login");
        let deleted = pair.parity(
            |port, session| request(port, "DELETE", account, &[session], b""),
            200,
            |reply| reply,
        )
        .await;
        assert_eq!(deleted, json!({"status":"ok"}));
        assert_eq!(pair.read("/api/accounts").await["accounts"], json!([]));
    });
    old.legacy.shutdown().unwrap();
}

#[test]
fn account_writes_match_legacy_wire_and_persist_the_same_state() {
    let upstream = TcpListener::bind(LOOPBACK).unwrap();
    upstream.set_nonblocking(true).unwrap();
    let executable = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let codex = executable.path().join("codex");
    fake_codex(&codex);
    let old = fixture(upstream.local_addr().unwrap(), &codex);
    fixture(upstream.local_addr().unwrap(), &codex).run_candidate(async |new, candidate| {
        let pair = Pair { old: &old, new, candidate };
        let upstream = AsyncListener::from_std(upstream).unwrap();

        for path in [IMPORT, CONFIG, MODELS, QUOTA, RESET] {
            pair.parity(|port, _| json_request(port, path, &[], b"{"), 401, |reply| reply).await;
            assert!(pair.post(path, b"{", 400).await["error"]["message"].as_str().unwrap().contains("valid JSON"));
            assert_eq!(pair.post(path, b"[]", 400).await["error"]["message"], "request body must be a JSON object");
            let oversized = pair.parity(|port, session| request(
                port, "POST", path, &[session, "Content-Type: application/json", "Content-Length: 5242881"], b""
            ), 413, |reply| reply).await;
            assert_eq!(oversized["error"]["limit_bytes"], 5242880);
        }
        assert_eq!(pair.post(IMPORT, b"{}", 400).await["error"]["message"], "auth_json is required");
        assert_eq!(pair.post(IMPORT, br#"{"auth_json":true}"#, 400).await["error"]["message"], "auth_json must be a JSON object");
        pair.post(CONFIG, br#"{"accounts":[{"id":"absent","prefix":"absent","auth_file":"unmanaged"}]}"#, 400).await;
        assert_eq!(pair.post(MODELS, b"{}", 400).await["error"]["message"], "Subscription account is unavailable");
        for path in [QUOTA, RESET] {
            let missing = pair.post(path, b"{}", 503).await;
            assert_eq!(missing["error"]["message"], "unknown account: demo");
        }
        pair.parity(|port, _| request(port, "DELETE", ACCOUNT, &[], b""), 401, |reply| reply).await;
        pair.parity(|port, session| request(port, "DELETE", "/api/accounts/", &[session], b""), 404, |reply| reply).await;
        pair.parity(|port, session| request(port, "DELETE", ACCOUNT, &[session], b""), 400, |reply| reply).await;

        let oversized = gzip(&vec![b' '; 5 * 1024 * 1024 + 1], Vec::new());
        let rejected = pair
            .parity(
                |port, session| {
                    json_request(port, IMPORT, &[session, "Content-Encoding: gzip"], &oversized)
                },
                413,
                |reply| reply,
            )
            .await;
        assert_eq!(rejected["error"]["limit_bytes"], 5242880);

        let imported = pair.post(IMPORT, IMPORT_BODY, 200).await;
        assert_eq!(imported["account"]["name"], "Initial");
        assert_eq!(pair.saved()[0]["name"], "Initial");
        for fixture in [&old, new] {
            let path = fixture.directory.path().canonicalize().unwrap().join(AUTH);
            let auth = fixture.legacy.state.backend.configuration.vault.read_encrypted_json(&path).unwrap();
            assert_eq!(auth["tokens"]["access_token"], "fixture-token");
        }
        let (models, ()) = tokio::join!(
            pair.post(MODELS, br#"{"other":7}"#, 200),
            async {
                let (first, second) = tokio::join!(wait_for_model_fetch(&upstream), wait_for_model_fetch(&upstream));
                tokio::join!(send_models(first, "persisted-model"), send_models(second, "persisted-model"));
            }
        );
        assert_eq!(models, pair.read(ACCOUNT_READ).await);
        assert_eq!(models["models"][0]["id"], "persisted-model");
        let config = json!({
            "codex_base_url": format!("http://{}/v1", upstream.local_addr().unwrap()),
            "accounts":[{"id":"demo","prefix":"demo","name":"Edited","hidden_models":["hidden"],
                         "model_context_windows":{"persisted-model":8192}}],
            "unknown":true,
        }).to_string();
        let edited = pair.parity(
            |port, session| json_request(port, CONFIG, &[session], config.as_bytes()),
            200,
            |reply| normalize_body(reply, |body| {
                [&old, new].iter().fold(body.to_owned(), |body, fixture| {
                    body.replace(fixture.directory.path().canonicalize().unwrap().to_str().unwrap(), "/fixture")
                })
            }),
        ).await;
        assert_eq!(edited["accounts"][0]["name"], "Edited");
        let saved = pair.saved();
        assert_eq!(saved[0]["hidden_models"], json!(["hidden"]));
        assert_eq!(saved[0]["model_context_windows"]["persisted-model"], 8192);
        assert_eq!(pair.read("/api/accounts").await["accounts"][0]["name"], "Edited");

        let invalid = pair.post(RESET, br#"{"credit_id":null}"#, 400).await;
        assert_eq!(invalid["error"]["message"], "reset credit id is invalid");
        assert_eq!(pair.post(RESET, b"{}", 400).await["error"]["code"], "quota_reset_invalid_request");
        for path in [QUOTA, RESET] {
            let body = if path == RESET { RESET_BODY } else { br#"{"other":true}"# };
            let reply = pair.parity(
                |port, session| json_request(port, path, &[session], body),
                200,
                normalize_quota_time,
            ).await;
            assert_eq!(reply["account"]["quota"]["rate_limits"]["primary"]["usedPercent"], 7);
            assert_eq!(pair.saved()[0]["quota"]["rate_limits"]["primary"]["usedPercent"], 7);
            if path == RESET {
                assert_eq!(reply["outcome"], "reset");
                assert!(reply["refresh_error"].is_null());
            }
        }

        let deleted = pair.parity(|port, session| request(
            port, "DELETE", "/api/accounts/%64emo//", &[session, "Content-Type: text/plain", "Content-Length: 1"], b"{"
        ), 200, |reply| reply).await;
        assert_eq!(deleted, json!({"status":"ok"}));
        assert_eq!(pair.saved(), json!([]));
        assert_eq!(pair.read("/api/accounts").await["accounts"], json!([]));
        for fixture in [&old, new] {
            assert!(matches!(std::fs::metadata(fixture.directory.path().join(AUTH)), Err(error) if error.kind() == ErrorKind::NotFound));
            assert!(fixture.legacy.state.updates.drain_for_test(Duration::ZERO));
        }
        for path in [IMPORT, CONFIG, MODELS, QUOTA, RESET] {
            let denied = pair.parity(|port, _| json_request(port, path, &[], b"{"), 503, |reply| reply).await;
            assert_eq!(denied["error"]["code"], "updating");
        }
        let denied = pair.parity(|port, _| request(port, "DELETE", ACCOUNT, &[], b""), 503, |reply| reply).await;
        assert_eq!(denied["error"]["code"], "updating");
        assert_eq!(pair.read("/api/accounts").await["accounts"], json!([]));
        assert_eq!(pair.saved(), json!([]));
    });
    old.legacy.shutdown().unwrap();
}

#[test]
fn submitted_account_model_refresh_finishes_once_after_disconnect() {
    let upstream = TcpListener::bind(LOOPBACK).unwrap();
    upstream.set_nonblocking(true).unwrap();
    let executable = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let codex = executable.path().join("codex");
    fake_codex(&codex);
    fixture(upstream.local_addr().unwrap(), &codex).run_candidate(async |fixture, candidate| {
        let upstream = AsyncListener::from_std(upstream).unwrap();
        let session = &fixture.session();
        let port = fixture.port();
        let import = json_request(port, IMPORT, &[session], IMPORT_BODY);
        assert_status(&exchange_wire(candidate, &import, true).await, 200);
        let refresh = json_request(port, MODELS, &[session], b"{}");
        let (mut client, task) = connect_submitted_write(fixture, candidate).await;
        client.write_all(&refresh).await.unwrap();
        let upstream_request = wait_for_model_fetch(&upstream).await;
        assert!(!fixture.legacy.state.updates.drain_for_test(Duration::ZERO));
        disconnect_submitted_write(fixture, client, task).await;
        send_models(upstream_request, "after-disconnect").await;

        let state = fixture.legacy.state.clone();
        tokio::task::spawn_blocking(move || {
            assert!(
                state
                    .connection_admission
                    .quiesce(0, Duration::from_secs(5))
                    .is_some()
            );
        })
        .await
        .unwrap();

        let read = request(port, "GET", ACCOUNT_READ, &[session], b"");
        let reply = exchange_wire(candidate, &read, false).await;
        assert_status(&reply, 200);
        assert_eq!(json_body(&reply)["models"][0]["id"], "after-disconnect");
        assert!(timeout(Duration::ZERO, upstream.accept()).await.is_err());
    });
}
