//! Real server contract tests.
use super::*;

#[test]
fn native_catalog_model_reaches_the_native_transport_boundary() {
    let upstream = OneShotUpstream::start(json!({
        "id":"resp_native_fixture","object":"response","status":"completed",
        "model":"gpt-native-fixture","output":[]
    }));
    let directory = tempfile::tempdir().expect("temporary directory");
    let root = canonical_root(&directory);
    let catalog = root.join("models_cache.json");
    std::fs::write(
        &catalog,
        serde_json::to_vec(&json!({
            "models": [{
                "slug": "gpt-native-fixture",
                "context_window": 272000,
                "supported_in_api": true
            }]
        }))
        .expect("encode native catalog"),
    )
    .expect("write native catalog");
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "native_catalog_path": catalog,
            "codex_base_url": upstream.base_url(),
            "providers":[{
                "id":"native-forward","base_url":upstream.base_url(),
                "protocol":"responses","auth_mode":"forward"
            }]
        }))
        .expect("encode config"),
    )
    .expect("write config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start native catalog server");
    let body = serde_json::to_vec(&json!({
        "model": "gpt-native-fixture",
        "input": "hello",
        "stream": false
    }))
    .expect("request JSON");
    let response = post(
        &server,
        "/v1/responses",
        &body,
        &[
            &session_cookie_header(&server),
            "Authorization: Bearer native-fixture",
        ],
    );
    assert!(
        response.starts_with("HTTP/1.1 200 OK\r\n"),
        "catalog model must cross the native transport boundary: {response}"
    );
    let (path, headers, body) = upstream.observed();
    assert_eq!(path, "/v1/responses");
    assert_eq!(headers["content-encoding"], "zstd");
    assert_eq!(headers["authorization"], "Bearer native-fixture");
    assert_eq!(body["model"], "gpt-native-fixture");
    server.shutdown().expect("shutdown");
}

#[test]
fn quota_events_are_bounded_authenticated_and_revision_driven() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let config = canonical_root(&directory).join("config.json");
    std::fs::write(&config, b"{}").expect("write config");
    let server = ServerHandle::start_with_config(IpAddr::V4(Ipv4Addr::LOCALHOST), 0, &config)
        .expect("start quota event server");
    let cookie = session_cookie_header(&server);

    let denied = request(&server, "/api/accounts/events", &[]);
    assert!(
        denied.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{denied}"
    );
    let cross_origin = request(
        &server,
        "/api/accounts/events",
        &[&cookie, "Origin: https://example.invalid"],
    );
    assert!(
        cross_origin.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{cross_origin}"
    );

    let mut streams = (0..QUOTA_EVENT_SLOT_LIMIT)
        .map(|_| open_quota_events(&server, &cookie))
        .collect::<Vec<_>>();
    let excess = request(&server, "/api/accounts/events", &[&cookie]);
    assert!(
        excess.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{excess}"
    );
    assert!(excess.contains("Retry-After: 15\r\n"));

    notify_quota_update(&server.state, "@native", Some("quota_auth_required"));
    assert_eq!(
        read_sse_frame(&mut streams[0]),
        "event: quota-updated\ndata: {}\n"
    );
    let accounts = request(&server, "/api/accounts", &[&cookie]);
    assert!(accounts.starts_with("HTTP/1.1 200 OK\r\n"), "{accounts}");
    let accounts: Value = serde_json::from_str(
        accounts
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("account state");
    assert_eq!(accounts["refresh_errors"]["@native"], "quota_auth_required");

    notify_quota_update(&server.state, "@native", None);
    assert_eq!(
        read_sse_frame(&mut streams[0]),
        "event: quota-updated\ndata: {}\n"
    );
    server.shutdown().expect("shutdown");
    let mut tail = Vec::new();
    streams[0]
        .read_to_end(&mut tail)
        .expect("quota stream closes during shutdown");
}

#[cfg(unix)]
#[test]
fn native_and_imported_quota_refresh_cross_the_management_boundary() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let root = canonical_root(&directory);
    let auth_path = root.join("auth.json");
    let original_auth = serde_json::to_vec(&json!({
        "tokens": {"access_token": "native-secret", "account_id": "workspace"}
    }))
    .expect("encode auth");
    std::fs::write(&auth_path, &original_auth).expect("write native auth");
    let account_root = root.join("state").join("accounts");
    let imported_auth = account_root.join("egg").join("auth.json.enc");
    let duplicate_auth = account_root.join("native-copy").join("auth.json.enc");
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "native_hidden_models": ["gpt-hidden"],
            "native_model_context_windows": {"gpt-visible": 200000},
            "account_store_path": account_root,
            "accounts": [{
                "id": "egg",
                "name": "egg",
                "prefix": "egg",
                "auth_file": imported_auth,
            }, {
                "id": "native-copy",
                "name": "Native copy",
                "prefix": "native-copy",
                "auth_file": duplicate_auth,
            }]
        }))
        .expect("encode config"),
    )
    .expect("write config");

    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("fake Codex directory");
    let executable = executable_root.path().join("codex");
    std::fs::write(
            &executable,
            r#"#!/usr/bin/env python3
import json, pathlib, os, sys
home = pathlib.Path(os.environ["CODEX_HOME"])
started = ""
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id": request["id"], "result": {}}), flush=True)
    elif method == "account/read":
        auth = json.loads((home / "auth.json").read_text())
        started = auth["tokens"]["access_token"]
        if started == "imported-original":
            assert request["params"] == {"refreshToken": False}
            auth["tokens"]["access_token"] = "imported-rotated"
            (home / "auth.json").write_text(json.dumps(auth))
        elif started == "imported-rotated":
            assert request["params"] in ({"refreshToken": False}, {"refreshToken": True})
        print(json.dumps({"id": request["id"], "result": {"account": {"email": "xian@example.com", "planType": "pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        if started == "imported-original":
            print(json.dumps({"id": request["id"], "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 401 Unauthorized; content-type=text/plain; body=private-token"}}), flush=True)
        else:
            used = 11 if started == "imported-rotated" else 7
            if started == "native-secret":
                auth = json.loads((home / "auth.json").read_text())
                auth["tokens"]["access_token"] = "isolated-rotation"
                (home / "auth.json").write_text(json.dumps(auth))
            buckets = {"codex": {
                "limitId": "codex",
                "primary": {"usedPercent": used, "windowDurationMins": 300, "resetsAt": 123},
                "secondary": {"usedPercent": 40, "windowDurationMins": 10080, "resetsAt": 456},
            }}
            if started == "imported-rotated":
                buckets["free"] = {"primary": {"usedPercent": 12, "windowDurationMins": 43200, "resetsAt": 789}}
            result = {
                "rateLimitsByLimitId": buckets,
                "rateLimitResetCredits": {"availableCount": 2, "credits": [{"id": "opaque-reset-id", "status": "available", "expiresAt": 1000, "title": "Full reset"}]},
            }
            print(json.dumps({"id": request["id"], "result": result}), flush=True)
    elif method == "account/rateLimitResetCredit/consume":
        expected = {"idempotencyKey": "12345678-1234-4123-8123-123456789abc"}
        if started == "native-secret":
            expected["creditId"] = "opaque-reset-id"
        assert request["params"] == expected
        print(json.dumps({"id": request["id"], "result": {"outcome": "reset"}}), flush=True)
"#,
        )
        .expect("write fake Codex");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("make fake Codex executable");

    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        executable.to_str().expect("UTF-8 executable path"),
        auth_path.clone(),
    )
    .expect("start quota server");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &imported_auth,
            &json!({
                "tokens": {
                    "access_token": "imported-original",
                    "account_id": "workspace-egg"
                }
            }),
        )
        .expect("write imported auth");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &duplicate_auth,
            &json!({
                "tokens": {
                    "access_token": "stale-native-snapshot",
                    "account_id": "workspace"
                }
            }),
        )
        .expect("write duplicate auth");
    let cookie = session_cookie_header(&server);
    let before = request(&server, "/api/accounts", &[&cookie]);
    assert!(before.starts_with("HTTP/1.1 200 OK\r\n"), "{before}");
    let before: Value =
        serde_json::from_str(before.split_once("\r\n\r\n").expect("response separator").1)
            .expect("account snapshot");
    assert_eq!(before["native_account"]["credential_set"], true);
    assert_eq!(before["native_account"]["quota"], Value::Null);

    let unauthorized = post(&server, "/api/accounts/%40native/quota", b"{}", &[]);
    assert!(
        unauthorized.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{unauthorized}"
    );
    let unknown = post(&server, "/api/accounts/missing/quota", b"{}", &[&cookie]);
    assert!(
        unknown.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{unknown}"
    );
    let unknown: Value = serde_json::from_str(
        unknown
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("unknown account error");
    assert_eq!(
        unknown,
        json!({"error":{"code":"quota_error","message":"unknown account: missing"}})
    );

    let refreshed = post(&server, "/api/accounts/%40native/quota", b"{}", &[&cookie]);
    assert!(refreshed.starts_with("HTTP/1.1 200 OK\r\n"), "{refreshed}");
    let refreshed: Value = serde_json::from_str(
        refreshed
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("refreshed account");
    assert_eq!(refreshed["account"]["quota"]["plan_type"], "pro");
    assert_eq!(
        refreshed["account"]["quota"]["rate_limits"]["primary"]["usedPercent"],
        7
    );
    assert_eq!(
        refreshed["account"]["quota"]["rate_limits"]["primary"]["windowDurationMins"],
        300
    );
    assert_eq!(
        refreshed["account"]["quota"]["rate_limits"]["secondary"]["windowDurationMins"],
        10080
    );
    assert_eq!(
        refreshed["account"]["quota"]["credits"]["reset_credits"]["available_count"],
        2
    );
    assert_eq!(
        refreshed["account"]["quota"]["credits"]["reset_credits"]["credits"][0]["id"],
        "opaque-reset-id"
    );
    assert_eq!(
        std::fs::read(&auth_path).expect("native auth after refresh"),
        original_auth,
        "native quota refresh must never persist isolated token rotation"
    );

    let reset_body = serde_json::to_vec(&json!({
        "idempotency_key": "12345678-1234-4123-8123-123456789ABC",
        "credit_id": "opaque-reset-id"
    }))
    .expect("reset request");
    let reset = post(
        &server,
        "/api/accounts/%40native/quota-reset",
        &reset_body,
        &[&cookie],
    );
    assert!(reset.starts_with("HTTP/1.1 200 OK\r\n"), "{reset}");
    let reset: Value =
        serde_json::from_str(reset.split_once("\r\n\r\n").expect("response separator").1)
            .expect("reset response");
    assert_eq!(reset["outcome"], "reset");
    assert_eq!(reset["account"]["quota"]["plan_type"], "pro");
    assert_eq!(reset["refresh_error"], Value::Null);
    assert_eq!(
        reset["account"]["quota"]["credits"]["reset_credits"]["credits"][0]["title"],
        "Full reset"
    );
    assert_eq!(
        reset["account"]["quota"]["credits"]["reset_credits"]["credits"][0]["id"],
        "opaque-reset-id"
    );

    let invalid_reset = post(
        &server,
        "/api/accounts/%40native/quota-reset",
        br#"{"idempotency_key":"retry-me"}"#,
        &[&cookie],
    );
    assert!(
        invalid_reset.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{invalid_reset}"
    );
    assert!(invalid_reset.contains("quota_reset_invalid_request"));

    for invalid_credit_id in [json!(null), json!("   "), json!("x".repeat(257))] {
        let invalid_body = serde_json::to_vec(&json!({
            "idempotency_key": "12345678-1234-4123-8123-123456789ABC",
            "credit_id": invalid_credit_id,
        }))
        .expect("invalid credit request");
        let invalid = post(
            &server,
            "/api/accounts/%40native/quota-reset",
            &invalid_body,
            &[&cookie],
        );
        assert!(
            invalid.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{invalid}"
        );
        assert!(invalid.contains("quota_reset_invalid_request"));
    }

    let imported = post(&server, "/api/accounts/egg/quota", b"{}", &[&cookie]);
    assert!(imported.starts_with("HTTP/1.1 200 OK\r\n"), "{imported}");
    let imported: Value = serde_json::from_str(
        imported
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("imported account response");
    assert_eq!(imported["account"]["credential_status"], "valid");
    assert_eq!(
        imported["account"]["quota"]["rate_limits"]["primary"]["usedPercent"],
        11
    );
    assert_eq!(imported["account"]["quota"]["plan_type"], "free");
    assert_eq!(
        imported["account"]["quota"]["rate_limits"]["secondary"]["windowDurationMins"],
        10080
    );
    assert_eq!(
        imported["account"]["quota"]["rate_limits_by_limit_id"]["free"]["primary"]["windowDurationMins"],
        43200
    );
    let persisted_auth = server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(&imported_auth)
        .expect("read rotated imported auth");
    assert_eq!(
        persisted_auth["tokens"]["access_token"], "imported-rotated",
        "a rotation completed before the first 401 must be reused by the retry"
    );

    let imported_reset = post(
        &server,
        "/api/accounts/egg/quota-reset",
        br#"{"idempotency_key":"12345678-1234-4123-8123-123456789ABC"}"#,
        &[&cookie],
    );
    assert!(
        imported_reset.starts_with("HTTP/1.1 200 OK\r\n"),
        "{imported_reset}"
    );
    let imported_reset: Value = serde_json::from_str(
        imported_reset
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("imported reset response");
    assert_eq!(imported_reset["outcome"], "reset");
    assert_eq!(imported_reset["refresh_error"], Value::Null);
    assert_eq!(imported_reset["account"]["quota"]["plan_type"], "free");
    assert_eq!(
        imported_reset["account"]["quota"]["rate_limits"]["secondary"]["windowDurationMins"],
        10080
    );

    let duplicate = post(
        &server,
        "/api/accounts/native-copy/quota",
        b"{}",
        &[&cookie],
    );
    assert!(duplicate.starts_with("HTTP/1.1 200 OK\r\n"), "{duplicate}");
    let duplicate: Value = serde_json::from_str(
        duplicate
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("duplicate account response");
    assert_eq!(
        duplicate["account"]["quota"]["rate_limits"]["primary"]["usedPercent"], 7,
        "a duplicate account must query the live native credential"
    );
    assert_eq!(
        server
            .state
            .backend
            .configuration
            .vault
            .read_encrypted_json(&duplicate_auth)
            .expect("read duplicate snapshot")["tokens"]["access_token"],
        "stale-native-snapshot",
        "native refresh must not overwrite the imported snapshot"
    );

    let native_history = request(
        &server,
        "/api/accounts/%40native/quota-history?range=all",
        &[&cookie],
    );
    assert!(
        native_history.starts_with("HTTP/1.1 200 OK\r\n"),
        "{native_history}"
    );
    let native_history: Value = serde_json::from_str(
        native_history
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("native quota history");
    assert_eq!(native_history["account_id"], "@native");
    assert_eq!(
        native_history["series"][0]["points"][0]["remaining_percent"],
        93.0
    );
    assert_eq!(native_history["plans"][0]["plan_type"], "pro");

    let duplicate_history = request(
        &server,
        "/api/accounts/native-copy/quota-history?range=all",
        &[&cookie],
    );
    let duplicate_history: Value = serde_json::from_str(
        duplicate_history
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("duplicate quota history");
    assert_eq!(duplicate_history["account_id"], "native-copy");
    assert_eq!(duplicate_history["series"], native_history["series"]);

    let imported_history = request(
        &server,
        "/api/accounts/egg/quota-history?range=all",
        &[&cookie],
    );
    let imported_history: Value = serde_json::from_str(
        imported_history
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("imported quota history");
    assert_eq!(
        imported_history["series"][0]["points"][0]["remaining_percent"],
        89.0
    );

    let invalid_history = request(
        &server,
        "/api/accounts/egg/quota-history?range=forever",
        &[&cookie],
    );
    assert!(
        invalid_history.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{invalid_history}"
    );
    let missing_history = request(
        &server,
        "/api/accounts/missing/quota-history?range=all",
        &[&cookie],
    );
    assert!(
        missing_history.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{missing_history}"
    );
    assert_eq!(
        sample_quotas_once(&server.state),
        QuotaSampleCounts {
            sampled: 2,
            failed: 0,
        },
        "the sampler must refresh native and the unique imported account while skipping the duplicate"
    );
    server.shutdown().expect("shutdown");
}

#[cfg(unix)]
#[test]
fn quota_rate_limit_http_error_is_safe() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let root = canonical_root(&directory);
    let account_root = root.join("state").join("accounts");
    let auth_file = account_root.join("rate-limited").join("auth.json.enc");
    let config = root.join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec_pretty(&json!({
            "account_store_path": account_root,
            "accounts": [{
                "id": "rate-limited",
                "name": "Rate limited",
                "prefix": "rate-limited",
                "auth_file": auth_file,
            }]
        }))
        .expect("encode config"),
    )
    .expect("write config");

    let executable_root = tempfile::Builder::new()
        .prefix("emp-fake-codex-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("fake Codex directory");
    let executable = executable_root.path().join("codex");
    std::fs::write(
        &executable,
        r#"#!/usr/bin/env python3
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        print(json.dumps({"id": request["id"], "result": {}}), flush=True)
    elif method == "account/read":
        print(json.dumps({"id": request["id"], "result": {"account": {"email": "xian@example.com", "planType": "pro"}}}), flush=True)
    elif method == "account/rateLimits/read":
        print(json.dumps({"id": request["id"], "error": {"message": "failed to fetch codex rate limits: GET https://example.invalid failed: 429 Too Many Requests; content-type=text/plain; body=private-token"}}), flush=True)
"#,
    )
    .expect("write fake Codex");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("make fake Codex executable");

    let native_auth = root.join("auth.json");
    std::fs::write(&native_auth, b"{}").expect("write native auth");
    let server = ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        executable.to_str().expect("UTF-8 executable path"),
        native_auth,
    )
    .expect("start quota server");
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(
            &auth_file,
            &json!({
                "tokens": {
                    "access_token": "rate-limited",
                    "account_id": "workspace-rate-limited"
                }
            }),
        )
        .expect("write rate-limited auth");

    let cookie = session_cookie_header(&server);
    let response = post(
        &server,
        "/api/accounts/rate-limited/quota",
        b"{}",
        &[&cookie],
    );
    assert!(
        response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
        "{response}"
    );
    let body: Value = serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("response separator")
            .1,
    )
    .expect("rate-limit error response");
    assert_eq!(
        body,
        json!({"error":{"code":"quota_rate_limited","message":"Codex quota queries are rate limited (429); try again later"}})
    );
    assert!(!response.contains("private-token"));
    assert!(!response.contains("example.invalid"));
    server.shutdown().expect("shutdown");
}
