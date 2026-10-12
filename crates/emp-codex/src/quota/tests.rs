use super::projection::safe_reset_credits;
use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn helper_output_and_ignored_eof_are_bounded() {
    for oversized in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("fake-codex");
        let program = if oversized {
            "#!/usr/bin/env python3\nimport sys\nsys.stdout.write('x' * (5 * 1024 * 1024)); sys.stdout.flush()\n"
        } else {
            "#!/usr/bin/env python3\nimport json, sys, time\nfor line in sys.stdin:\n r = json.loads(line)\n if 'id' in r: print(json.dumps({'id':r['id'], 'result':{}}), flush=True)\ntime.sleep(60)\n"
        };
        fs::write(&script, program).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        let error = run_quota_query(
            &json!({"tokens":{"access_token":"fixture", "account_id":"fixture"}}),
            script.to_str().unwrap(),
            Duration::from_secs(3),
            false,
        )
        .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(8));
        assert_eq!(
            error.code(),
            if oversized {
                "quota_output_too_large"
            } else {
                "quota_error"
            }
        );
    }
}

#[test]
fn cancelling_a_quota_read_still_persists_a_rotated_credential() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("fake-codex");
    let marker = script.with_extension("ready");
    fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, os, pathlib, sys, time
for line in sys.stdin:
    r = json.loads(line)
    if 'id' not in r: continue
    if r['method'] == 'account/read':
        auth_path = pathlib.Path(os.environ['CODEX_HOME']) / 'auth.json'
        auth = json.loads(auth_path.read_text())
        auth['tokens']['access_token'] = 'rotated-fixture'
        auth_path.write_text(json.dumps(auth))
    if r['method'] == 'account/rateLimits/read':
        pathlib.Path(sys.argv[0]).with_suffix('.ready').write_text('ready')
        time.sleep(30)
    print(json.dumps({'id':r['id'], 'result':{}}), flush=True)
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let cancelled = AtomicBool::new(false);
    let auth = json!({"tokens":{"access_token":"fixture", "account_id":"fixture"}});
    let mut saved = None;
    thread::scope(|scope| {
        let worker = scope.spawn(|| {
            run_quota_query_persisting(
                &auth,
                script.to_str().unwrap(),
                QuotaControl {
                    timeout: Duration::from_secs(10),
                    cancelled: Some(&cancelled),
                },
                false,
                |auth| {
                    saved = Some(auth.clone());
                    Ok(())
                },
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists());
        let started = Instant::now();
        cancelled.store(true, Ordering::Release);
        assert_eq!(
            worker.join().unwrap().unwrap_err().code(),
            "quota_cancelled"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    });
    assert_eq!(saved.unwrap()["tokens"]["access_token"], "rotated-fixture");
}

#[test]
fn reset_credit_projection_preserves_only_usable_ids() {
    let source = json!({"credits": [
        {"id": " opaque ID ", "status": "available", "private": "drop"},
        {"id": "   ", "status": "available"},
        {"id": "x".repeat(257), "status": "available"},
        {"id": 42, "status": "available"},
    ]});
    let projected = safe_reset_credits(Some(&source)).expect("reset credits");
    assert_eq!(projected["credits"][0]["id"], " opaque ID ");
    assert!(projected["credits"][0].get("private").is_none());
    for index in 1..4 {
        assert!(projected["credits"][index].get("id").is_none());
    }
}

#[test]
fn isolated_process_sequences_requests_and_returns_rotated_auth() {
    let root = tempfile::Builder::new()
        .prefix("emp-fake-codex-")
        .tempdir()
        .expect("temporary root");
    let script = root.path().join("fake-codex");
    fs::write(
            &script,
            r#"#!/usr/bin/env python3
import configparser, json, os, pathlib, sys

home = pathlib.Path(os.environ["CODEX_HOME"])
assert pathlib.Path.cwd().samefile(home)
assert pathlib.Path(os.environ["HOME"]).samefile(home)
assert pathlib.Path(os.environ["USERPROFILE"]).samefile(home)
assert home.stat().st_mode & 0o077 == 0
assert (home / "auth.json").stat().st_mode & 0o077 == 0
with pathlib.Path(sys.argv[0]).with_suffix('.homes').open('a') as record:
    record.write(str(home) + '\n')
assert sys.argv[1:] == ["app-server", "--stdio"]
config = configparser.ConfigParser()
config.read_string("[root]\n" + (home / "config.toml").read_text())
assert config["root"]["cli_auth_credentials_store"] == '"file"'
assert config.getboolean("features", "plugins", fallback=True) is False
catalog_path = pathlib.Path(json.loads(config["root"]["model_catalog_json"]))
assert not catalog_path.is_absolute()
catalog_path = home / catalog_path
assert catalog_path.parent.samefile(home)
assert catalog_path.stat().st_mode & 0o077 == 0
assert json.loads(catalog_path.read_text())["models"]
requests = []
for line in sys.stdin:
    request = json.loads(line)
    requests.append(request)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id": request["id"], "result": {}}), flush=True)
    elif method == "initialized":
        pass
    elif method == "account/read":
        assert request["params"] == {"refreshToken": False}
        auth = json.loads((home / "auth.json").read_text())
        auth["tokens"]["access_token"] = "rotated-token"
        (home / "auth.json").write_text(json.dumps(auth))
        print(json.dumps({"id": request["id"], "result": {"account": {"email": "user@example.com", "planType": "pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        assert request["params"] is None
        print(json.dumps({"id": request["id"], "result": {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": 7}}}}), flush=True)
    elif method == "account/rateLimitResetCredit/consume":
        auth = json.loads((home / "auth.json").read_text())
        expected = {"idempotencyKey": "12345678-1234-4123-8123-123456789abc"}
        if auth["tokens"]["account_id"] == "selected":
            expected["creditId"] = "opaque selected id"
        assert request["params"] == expected
        print(json.dumps({"id": request["id"], "result": {"outcome": "reset"}}), flush=True)
"#,
        )
        .expect("write fake Codex");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))
        .expect("make fake Codex executable");
    let auth = json!({"tokens": {"access_token": "original-token", "account_id": "a"}});
    let trusted = TrustedBinary::resolve(script.to_str().expect("UTF-8 fake Codex path"))
        .expect("trusted fake Codex");
    let result = run_isolated_quota_process(
        &auth,
        &trusted,
        Duration::from_secs(5),
        false,
        None,
        None,
        None,
    )
    .expect("isolated quota query");
    assert_eq!(result.quota["account_label"], "u***@example.com");
    assert_eq!(result.quota["plan_type"], "pro");
    assert_eq!(result.quota["rate_limits"]["primary"]["usedPercent"], 7);
    assert_eq!(
        result.refreshed_auth.expect("rotated auth")["tokens"]["access_token"],
        "rotated-token"
    );
    assert_eq!(
        run_quota_reset(
            &auth,
            script.to_str().expect("UTF-8 fake Codex path"),
            Duration::from_secs(5),
            false,
            "12345678-1234-4123-8123-123456789ABC",
            None,
        )
        .expect("quota reset"),
        "reset"
    );
    let mut selected_auth = auth.clone();
    selected_auth["tokens"]["account_id"] = Value::String("selected".to_owned());
    assert_eq!(
        run_quota_reset(
            &selected_auth,
            script.to_str().expect("UTF-8 fake Codex path"),
            Duration::from_secs(5),
            false,
            "12345678-1234-4123-8123-123456789ABC",
            Some("opaque selected id"),
        )
        .expect("selected quota reset"),
        "reset"
    );
    let homes = fs::read_to_string(script.with_extension("homes")).unwrap();
    let homes: std::collections::BTreeSet<_> = homes.lines().collect();
    assert_eq!(homes.len(), 3, "each helper must have its own private home");
    for home in homes {
        assert!(!Path::new(home).exists(), "helper home was not removed");
    }
    let invalid =
        validated_reset_idempotency_key("retry-me").expect_err("non-UUID idempotency key");
    assert_eq!(invalid.code(), "quota_reset_invalid_request");
    let too_long = "x".repeat(257);
    for invalid in ["", " ", too_long.as_str()] {
        assert_eq!(
            validated_reset_credit_id(Some(invalid))
                .expect_err("invalid reset credit id")
                .code(),
            "quota_reset_invalid_request"
        );
    }
}

#[test]
fn native_snapshot_has_no_refresh_credential_and_does_not_rewrite_login() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("auth.json");
    let bytes = serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{
        "access_token":"native-access","refresh_token":"native-refresh","account_id":"native-account"},
        "last_refresh":"2020-01-01T00:00:00Z"})).unwrap();
    fs::write(&path, &bytes).unwrap();
    let snapshot = read_native_auth(&path).unwrap();
    assert_eq!(snapshot["auth_mode"], "chatgptAuthTokens");
    assert_eq!(snapshot["tokens"]["refresh_token"], "");
    assert_eq!(snapshot["tokens"]["access_token"], "native-access");
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn quota_helper_accepts_temp_roots_that_cannot_be_toml_literals() {
    use std::os::unix::ffi::OsStringExt;
    if let Some(binary) = env::var_os("EMP_QUOTA_PATH_FIXTURE") {
        let quota = run_quota_query(
            &json!({"tokens":{"access_token":"fixture"}}),
            binary.to_str().unwrap(),
            Duration::from_secs(5),
            false,
        )
        .unwrap()
        .quota;
        assert_eq!(quota["rate_limits"]["primary"]["usedPercent"], 7);
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("fake-codex");
    fs::write(&script, r#"#!/usr/bin/env python3
import configparser, json, os, pathlib, sys
home = pathlib.Path(os.environ['CODEX_HOME'])
# The helper's scalar TOML subset is INI-compatible; Python 3.10 has this parser.
config = configparser.ConfigParser(interpolation=None)
config.read_string('[quota_fixture]\n' + (home / 'config.toml').read_text())
catalog = json.loads(config['quota_fixture']['model_catalog_json'])
assert not pathlib.Path(catalog).is_absolute()
assert json.loads((home / catalog).read_text())['models']
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request: continue
    result = {'rateLimits':{'primary':{'usedPercent':7}}} if request['method'] == 'account/rateLimits/read' else {}
    print(json.dumps({'id':request['id'],'result':result}), flush=True)
"#).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    for name in [b"del-\x7f".to_vec(), b"non-utf8-\xff".to_vec()] {
        let temp_root = root.path().join(std::ffi::OsString::from_vec(name));
        fs::create_dir(&temp_root).unwrap();
        let result = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "quota::tests::quota_helper_accepts_temp_roots_that_cannot_be_toml_literals",
            ])
            .env("EMP_QUOTA_PATH_FIXTURE", &script)
            .env("TMPDIR", &temp_root)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        assert_eq!(
            fs::read_dir(temp_root).unwrap().count(),
            0,
            "helper credentials left behind"
        );
    }
}
