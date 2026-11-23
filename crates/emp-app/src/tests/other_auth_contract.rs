#![cfg(unix)]

use super::*;
use crate::services::accounts::{account_allows_refresh, import_emp_login_account_state};
use crate::services::quota::{consume_quota_reset_for_account, refresh_account_serialized};
use std::os::unix::fs::PermissionsExt;

struct QuotaFixture {
    server: ServerHandle,
    auth_path: PathBuf,
    engine: TempDir,
    _directory: TempDir,
}

impl QuotaFixture {
    fn start(source: Option<&str>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = credential_root(&directory);
        let config_path = root.join("config.json");
        let auth_path = root.join("state/accounts/demo/auth.json.enc");
        let mut account = json!({"id":"demo","name":"Demo","prefix":"demo","auth_file":auth_path});
        if let Some(source) = source {
            account["credential_source"] = source.into();
        }
        std::fs::write(&config_path, json!({"accounts":[account]}).to_string()).unwrap();
        let engine = tempfile::Builder::new()
            .prefix("emp-other-auth-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let script = engine.path().join("codex");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
import json, os, pathlib, sys
root = pathlib.Path(__file__).parent
home = pathlib.Path(os.environ['CODEX_HOME'])
mode = (root / 'mode').read_text()
refresh = False
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'initialize':
        result = {}
    elif method == 'account/read':
        refresh = request['params']['refreshToken']
        auth = json.loads((home / 'auth.json').read_text())
        with (root / 'calls').open('a') as log:
            log.write(json.dumps({'auth':auth, 'refresh':refresh}) + '\n')
        if mode == 'routing':
            print(json.dumps({'id':request['id'], 'error':{'message':'workspace routing discovery failed'}}), flush=True)
            continue
        if refresh or mode == 'success':
            auth['tokens']['access_token'] = 'rotated-access'
            auth['tokens']['refresh_token'] = 'rotated-refresh'
            (home / 'auth.json').write_text(json.dumps(auth))
        result = {'account':{'email':'fixture@example.test','planType':'plus'}}
    elif method in ('account/rateLimits/read', 'account/rateLimitResetCredit/consume'):
        if mode == 'expired' or (mode == 'retry' and not refresh):
            print(json.dumps({'id':request['id'], 'error':{'message':'chatgpt authentication required to read rate limits'}}), flush=True)
            continue
        result = {'outcome':'reset'} if method.endswith('/consume') else {'rateLimits':{'primary':{'usedPercent':7}}}
    else:
        continue
    print(json.dumps({'id':request['id'], 'result':result}), flush=True)
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(engine.path().join("mode"), "success").unwrap();
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config_path,
            script.to_str().unwrap(),
            root.join("codex/auth.json"),
        )
        .unwrap();
        server
            .state
            .backend
            .configuration
            .vault
            .write_encrypted_json(
                &auth_path,
                &json!({
                    "auth_mode":"chatgpt", "refresh_token":"top-level-refresh",
                    "tokens":{"access_token":"original-access","refresh_token":"original-refresh","account_id":"fixture"},
                    "last_refresh":"2020-01-01T00:00:00Z"
                }),
            )
            .unwrap();
        Self {
            server,
            auth_path,
            engine,
            _directory: directory,
        }
    }

    fn mode(&self, mode: &str) {
        std::fs::write(self.engine.path().join("mode"), mode).unwrap();
    }

    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.engine.path().join("calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn reset(&self) -> Result<String, emp_codex::quota::QuotaError> {
        consume_quota_reset_for_account(
            &self.server.state,
            "demo",
            "12345678-1234-4123-8123-123456789abc",
            None,
        )
    }

    fn account(&self) -> Value {
        self.server.state.backend.configuration.snapshot().unwrap()["accounts"][0].clone()
    }
}

/// Fails if a shared quota read/reset exposes a refresh credential or saves a helper mutation.
#[test]
fn shared_quota_reads_and_resets_use_snapshots_without_persisting() {
    for (source, team_shared) in [
        (None, false),
        (Some("auth_json"), false),
        (Some("future-source"), false),
        (Some("emp_login"), true),
    ] {
        let fixture = QuotaFixture::start(source);
        if team_shared {
            fixture
                .server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap()["accounts"][0]["workspace_shared"] = true.into();
        }
        let original = std::fs::read(&fixture.auth_path).unwrap();
        let failing_path = fixture.auth_path.to_str().unwrap().to_owned();
        crate::services::quota::FAIL_ROTATION_SAVES_TO
            .lock()
            .unwrap()
            .insert(failing_path.clone());
        let read = refresh_account_serialized(&fixture.server.state, "demo");
        let reset = fixture.reset();
        crate::services::quota::FAIL_ROTATION_SAVES_TO
            .lock()
            .unwrap()
            .remove(&failing_path);
        assert!(read.is_ok(), "{read:?}");
        assert_eq!(reset.unwrap(), "reset");
        assert_eq!(fixture.account()["credential_status"], "valid");
        let calls = fixture.calls();
        assert_eq!(calls.len(), 2);
        for call in calls {
            assert_eq!(call["refresh"], false);
            assert_eq!(call["auth"]["auth_mode"], "chatgptAuthTokens");
            assert_eq!(call["auth"]["tokens"]["refresh_token"], "");
            assert!(call["auth"].get("refresh_token").is_none());
            assert_eq!(call["auth"]["tokens"]["access_token"], "original-access");
        }
        assert_eq!(std::fs::read(&fixture.auth_path).unwrap(), original);
        assert!(
            fixture
                .server
                .state
                .backend
                .accounts
                .pending_rotations
                .lock()
                .unwrap()
                .is_empty()
        );
        fixture.server.shutdown().unwrap();
    }
}

/// Fails if expired shared credentials trigger a refresh retry or remain marked usable.
#[test]
fn shared_quota_auth_failures_invalidate_without_retrying() {
    let fixture = QuotaFixture::start(None);
    let original = std::fs::read(&fixture.auth_path).unwrap();
    fixture.mode("expired");
    let error = refresh_account_serialized(&fixture.server.state, "demo").unwrap_err();
    assert_eq!(error.code(), "quota_auth_required");
    assert_eq!(fixture.account()["credential_status"], "invalid");
    fixture
        .server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"][0]["credential_status"] = "valid".into();
    assert_eq!(fixture.reset().unwrap_err().code(), "quota_auth_required");
    assert_eq!(fixture.account()["credential_status"], "invalid");
    fixture.mode("routing");
    let error = refresh_account_serialized(&fixture.server.state, "demo").unwrap_err();
    assert!(error.should_retry_imported_refresh());
    let calls = fixture.calls();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|call| call["refresh"] == false));
    assert_eq!(std::fs::read(&fixture.auth_path).unwrap(), original);
    fixture.server.shutdown().unwrap();
}

/// Fails if EMP-owned credentials lose the explicit refresh retry or durable rotation save.
#[test]
fn emp_login_retries_quota_reads_and_resets_and_saves_rotation() {
    for reset in [false, true] {
        let fixture = QuotaFixture::start(Some("emp_login"));
        fixture.mode("retry");
        if reset {
            assert_eq!(fixture.reset().unwrap(), "reset");
        } else {
            refresh_account_serialized(&fixture.server.state, "demo").unwrap();
        }
        let calls = fixture.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["refresh"], false);
        assert_eq!(calls[1]["refresh"], true);
        assert_eq!(
            calls[0]["auth"]["tokens"]["refresh_token"],
            "original-refresh"
        );
        let saved = fixture
            .server
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&fixture.auth_path)
            .unwrap();
        assert_eq!(saved["tokens"]["refresh_token"], "rotated-refresh");
        assert_eq!(fixture.account()["credential_source"], "emp_login");
        fixture.server.shutdown().unwrap();
    }
}

/// Fails if auth.json import can claim or retain EMP ownership, or login cannot restore it.
#[test]
fn credential_reimports_follow_the_current_source() {
    let fixture = QuotaFixture::start(None);
    assert!(!account_allows_refresh(&fixture.server.state, "demo"));
    let body = json!({"id":"demo","name":"Demo","prefix":"demo","credential_source":"emp_login",
        "auth_json":{"tokens":{"access_token":"replacement","account_id":"fixture"}}});
    let cookie = session_header(&fixture.server);
    let import = || {
        let response = post(
            &fixture.server,
            "/api/accounts/import",
            body.to_string().as_bytes(),
            &[&cookie],
        );
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(fixture.account().get("credential_source").is_none());
        assert!(!account_allows_refresh(&fixture.server.state, "demo"));
    };
    import();
    import_emp_login_account_state(&fixture.server.state, &body).unwrap();
    assert_eq!(fixture.account()["credential_source"], "emp_login");
    assert!(account_allows_refresh(&fixture.server.state, "demo"));
    import();
    import_emp_login_account_state(&fixture.server.state, &body).unwrap();
    assert!(account_allows_refresh(&fixture.server.state, "demo"));
    fixture.server.shutdown().unwrap();
}
