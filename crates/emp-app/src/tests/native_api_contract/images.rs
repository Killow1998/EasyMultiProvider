//! Real HTTP/WS turns and image endpoints against synthetic subscription accounts.
use super::support::*;
use super::*;
mod encoding;
mod fixture;
mod installed;
mod routing;
use fixture::*;

/// Fails if an image 401 refreshes a shared Other account instead of preserving rejection.
#[test]
fn shared_image_account_401_never_enters_credential_refresh() {
    let upstream = ImageUpstream::start(401);
    let (directory, server) = image_server(&upstream.base_url());
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["image_routing"] = json!({"mode":"account","account_id":"selected"});
    select_turn(&server, "selected/gpt-6.1-sol", "shared-image-turn", false);
    upstream.next();
    let auth_path =
        server.state.backend.configuration.snapshot().unwrap()["accounts"][0]["auth_file"]
            .as_str()
            .unwrap()
            .to_owned();
    let original = std::fs::read(&auth_path).unwrap();
    let response = post(
        &server,
        "/v1/images/generations",
        br#"{"model":"gpt-image-2","prompt":"fixture"}"#,
        &[
            &session_header(&server),
            "x-codex-image-turn-id: shared-image-turn",
        ],
    );
    assert!(
        response.starts_with("HTTP/1.1 401 Unauthorized"),
        "{response}"
    );
    assert_eq!(
        upstream.next().headers["authorization"],
        "Bearer selected-fixture"
    );
    assert!(upstream.observed.try_recv().is_err());
    assert_eq!(std::fs::read(&auth_path).unwrap(), original);
    server.shutdown().unwrap();
    let events = crate::tests::internal_events_contract::final_journal(directory.path());
    assert!(!events.iter().any(|event| event["event"] == "quota_refresh"));
}

#[test]
fn image_credit_uses_the_project_key_and_stops_dispatch_at_the_local_limit() {
    let upstream = super::vertex::VertexFixture::start_for("api.openai.com");
    let directory = tempfile::tempdir().unwrap();
    let root = crate::tests::credential_root(&directory);
    let home = root.join("codex");
    std::fs::create_dir_all(&home).unwrap();
    let path = root.join("config.json");
    let now = crate::util::system_now() as i64;
    let config = json!({"providers":[{"id":"team-openai","name":"Gift fixture","auth_mode":"api_key",
        "protocol":"responses","base_url":"https://api.openai.com/v1","api_key":"fixture-project-key"}],
        "image_routing":{"mode":"credit","credit":{"provider":"team-openai","model":"gpt-image-2.6-flare",
            "cycle_started_at":now-60,"expires_at":now+3600,"budget_cents":1,"percent":95}}});
    std::fs::write(&path, config.to_string()).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    std::fs::write(root.join("state/gateway-clients.json"), "{invalid fixture").unwrap();

    let server = ServerHandle::start_with_config_options_and_http_client(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &path,
        "missing-test-codex",
        home.join("auth.json"),
        upstream.client(),
    )
    .unwrap();
    let image = br#"{"model":"gpt-image-2","prompt":"synthetic"}"#;
    let url = "/v1/images/generations";
    let access = session_header(&server);
    assert!(
        !server
            .state
            .backend
            .modules
            .active(emp_modules::Module::Gateway)
    );
    let timestamp = crate::util::system_now();
    let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let head = format!(
        "POST {url} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\r\n",
        server.local_addr().port(),
        server.state.backend.gateway_key,
    );
    let image_request = crate::http::request::parse_request(&head).unwrap();
    assert!(
        crate::http::auth::proxy_identity(image_request, &server.state, timestamp, Some(peer))
            .is_none()
    );

    assert!(post(&server, url, image, &[]).starts_with("HTTP/1.1 401"));
    let wire = post(&server, url, image, &[&access]);
    assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
    let dispatched = upstream.received();
    assert_eq!(dispatched["path"], url);
    assert_eq!(
        dispatched["headers"]["authorization"],
        "Bearer fixture-project-key"
    );
    assert_eq!(dispatched["body"]["model"], "gpt-image-2.6-flare");
    let ledger = &server.state.backend.usage.ledger;
    assert_eq!(
        ledger.image_credit_cost("team-openai", now - 60).unwrap(),
        (12_068_000, 0)
    );
    let wire = post(
        &server,
        url,
        image,
        &[&access, "X-EMP-Provider: team-openai"],
    );
    assert!(wire.starts_with("HTTP/1.1 429"), "{wire}");
    let mut runtime_config = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap();
    runtime_config["image_routing"]["credit"]["cycle_started_at"] = json!(now - 45 * 86400);
    runtime_config["image_routing"]["credit"]["renew_monthly"] = json!(true);
    drop(runtime_config);
    let renewed = crate::services::native::images::credit_status(&server.state).unwrap();
    assert_eq!(renewed["active"], true);
    assert!(renewed["cycle_started_at"].as_i64().unwrap() > now - 45 * 86400);
    assert_eq!(renewed["estimated_spent_nanos"], 12_068_000);
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["image_routing"]["credit"]["budget_cents"] = json!(100);
    let interrupted = post(
        &server,
        url,
        br#"{"model":"gpt-image-2","prompt":"truncate-response"}"#,
        &[&access],
    );
    assert!(
        interrupted.starts_with("HTTP/1.1 502") || interrupted.starts_with("HTTP/1.1 503"),
        "{interrupted}"
    );
    assert_eq!(upstream.received()["path"], url);
    assert_eq!(
        ledger.image_credit_cost("team-openai", now - 60).unwrap(),
        (12_068_000, 1)
    );
    let paused = post(&server, url, image, &[&access]);
    assert!(
        paused.starts_with("HTTP/1.1 409") && paused.contains("did not report billable usage"),
        "{paused}"
    );
    let mut runtime_config = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap();
    runtime_config["image_routing"]["credit"]["expires_at"] = json!(now - 1);
    drop(runtime_config);
    let wire = post(&server, url, image, &[&access]);
    assert!(wire.starts_with("HTTP/1.1 409"), "{wire}");
    assert!(wire.contains("period is inactive"));
    server.shutdown().unwrap();
}

#[test]
fn codex_image_tool_uses_subscription_when_the_gift_period_has_expired() {
    let upstream = ImageUpstream::start(200);
    let (_directory, server) = image_server(&upstream.base_url());
    let now = crate::util::system_now() as i64;
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["image_routing"] = json!({"mode":"credit", "credit":{"provider":"unused-credit", "model":"gpt-image-2.6-flare",
            "cycle_started_at":now-3600,"expires_at":now-1,"budget_cents":1000,"percent":95}});
    select_turn(&server, "selected/gpt-6.1-sol", "gift-fallback-turn", false);
    upstream.next();
    let response = post(
        &server,
        "/v1/images/generations",
        br#"{"model":"gpt-image-2","prompt":"fixture"}"#,
        &[
            &session_header(&server),
            "x-codex-image-turn-id: gift-fallback-turn",
        ],
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let request = upstream.next();
    assert_eq!(request.path, "/backend-api/codex/images/generations");
    assert_eq!(request.body["model"], "gpt-image-2");
    assert!(request.headers.contains_key("chatgpt-account-id"));
    server.shutdown().unwrap();
}

#[test]
fn image_tools_follow_their_own_http_and_websocket_turn_accounts() {
    for websocket in [false, true] {
        let upstream = ImageUpstream::start(200);
        let (_directory, server) = image_server(&upstream.base_url());
        select_turn(&server, "selected/gpt-6.1-sol", "first-turn", websocket);
        assert_eq!(
            upstream.next().headers["authorization"],
            "Bearer selected-fixture"
        );
        select_turn(&server, "other/gpt-6.1-sol", "second-turn", false);
        upstream.next();
        for (operation, turn, account) in [
            ("generations", "first-turn", "selected"),
            ("edits", "second-turn", "other"),
        ] {
            let mut body = json!({"model":"gpt-image-2.6-sunburst","prompt":"synthetic prompt", "n":2, "background":"transparent", "output_format":"gif", "frame_count":2, "frame_duration_ms":200, "future":{"preserve":true}});
            if operation == "edits" {
                body["images"] = json!([{"image_url":"data:image/png;base64,ZmFrZQ=="},{"file_id":"file-fixture"}]);
            }
            let wire = post(
                &server,
                &format!("/v1/images/{operation}"),
                &serde_json::to_vec(&body).unwrap(),
                &[
                    &session_header(&server),
                    &format!("x-codex-image-turn-id: {turn}"),
                    "Authorization: Bearer wrong-native-fixture",
                    "chatgpt-account-id: native-owner",
                    "Cookie: must-not-forward",
                ],
            );
            assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
            assert_eq!(
                response_header(response_parts(&wire).0, "x-codex-imagegen-request-id"),
                Some("image-request-fixture"),
                "{wire}"
            );
            assert!(!wire.to_ascii_lowercase().contains("set-cookie"));
            let observed = upstream.next();
            assert_eq!(
                observed.path,
                format!("/backend-api/codex/images/{operation}")
            );
            assert_eq!(
                observed.headers["authorization"],
                format!("Bearer {account}-fixture")
            );
            assert_eq!(
                observed.headers["chatgpt-account-id"],
                format!("{account}-owner")
            );
            assert_eq!(observed.headers["x-codex-image-turn-id"], turn);
            assert!(!observed.headers.contains_key("cookie"));
            assert_eq!(observed.body, body);
            let result: Value = serde_json::from_slice(response_parts(&wire).1).unwrap();
            assert_eq!(result["data"][0]["generation_id"], "generation_fixture");
            assert_eq!(result["future"]["opaque"], true);
        }
        let activity = server
            .state
            .backend
            .activity
            .snapshot(crate::util::system_now() as u64);
        for (account, turn) in [("selected", "first-turn"), ("other", "second-turn")] {
            let record = activity["requests"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["model_id"] == format!("{account}/gpt-image-2.6-sunburst"))
                .unwrap();
            assert_eq!(record["turn_id"], turn);
            assert_eq!(record["account_id"], account);
            assert_eq!(record["input_tokens"], 4);
            assert_eq!(record["output_tokens"], 2);
            assert_eq!(record["state"], "completed");
        }
        let now = crate::util::system_now();
        let usage = server
            .state
            .backend
            .usage
            .ledger
            .query(0.0, now, "all", now, None)
            .unwrap();
        assert_eq!(usage["totals"]["input_tokens"], 8, "{usage}");
        assert_eq!(usage["totals"]["output_tokens"], 4, "{usage}");
        server.shutdown().unwrap();
    }
}

#[test]
fn image_tools_keep_upstream_quota_and_error_metadata_without_retrying() {
    for status in [401, 404, 429] {
        let upstream = ImageUpstream::start(status);
        let (_directory, server) = image_server(&upstream.base_url());
        select_turn(&server, "selected/gpt-6.1-sol", "limited-turn", false);
        upstream.next();
        let wire = post(
            &server,
            "/v1/images/generations",
            br#"{"model":"gpt-image-2.5-flare","prompt":"fixture"}"#,
            &[
                &session_header(&server),
                "x-codex-image-turn-id: limited-turn",
            ],
        );
        assert!(wire.starts_with(&format!("HTTP/1.1 {status}")), "{wire}");
        assert_eq!(
            response_header(response_parts(&wire).0, "retry-after"),
            Some("17"),
            "{wire}"
        );
        assert_eq!(
            response_header(response_parts(&wire).0, "x-codex-active-limit"),
            Some("image_gen"),
            "{wire}"
        );
        assert_eq!(
            response_header(response_parts(&wire).0, "x-image-gen-primary-used-percent"),
            Some("100"),
            "{wire}"
        );
        assert_eq!(
            response_header(response_parts(&wire).0, "x-codex-imagegen-request-id"),
            Some("image-request-fixture"),
            "{wire}"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(response_parts(&wire).1).unwrap()["error"]["type"],
            "usage_limit_reached"
        );
        upstream.next();
        assert!(
            upstream
                .observed
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );
        server.shutdown().unwrap();
    }
}

#[test]
fn image_tools_reject_unknown_turns_disabled_accounts_and_unauthorized_callers() {
    let upstream = ImageUpstream::start(200);
    let (_directory, server) = image_server(&upstream.base_url());
    let body = br#"{"model":"gpt-image-2.5-sunburst","prompt":"fixture"}"#;
    for (headers, status) in [
        (vec![], 401),
        (
            vec![
                session_header(&server),
                "Origin: https://untrusted.invalid".into(),
            ],
            403,
        ),
        (vec![session_header(&server)], 400),
        (
            vec![
                session_header(&server),
                "x-codex-image-turn-id: unknown".into(),
            ],
            409,
        ),
    ] {
        let headers = headers.iter().map(String::as_str).collect::<Vec<_>>();
        let wire = post(&server, "/v1/images/edits", body, &headers);
        assert!(wire.starts_with(&format!("HTTP/1.1 {status}")), "{wire}");
    }
    select_turn(&server, "selected/gpt-6.1-sol", "removed-turn", false);
    upstream.next();
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"][0]["enabled"] = json!(false);
    let wire = post(
        &server,
        "/v1/images/generations",
        body,
        &[
            &session_header(&server),
            "x-codex-image-turn-id: removed-turn",
        ],
    );
    assert!(wire.starts_with("HTTP/1.1 409"), "{wire}");
    assert!(
        upstream
            .observed
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    server.shutdown().unwrap();
}

#[test]
fn image_tools_keep_the_native_account_for_native_turns() {
    let upstream = ImageUpstream::start(200);
    let (_directory, server) = image_server(&upstream.base_url());
    select_turn(&server, "gpt-6.1-sol", "native-turn", false);
    assert_eq!(
        upstream.next().headers["authorization"],
        "Bearer native-fixture"
    );
    for _ in 0..2 {
        let wire = post(
            &server,
            "/v1/images/generations",
            br#"{"model":"gpt-image-2","prompt":"fixture"}"#,
            &[
                &session_header(&server),
                "x-codex-image-turn-id: native-turn",
                "Authorization: Bearer wrong-fixture",
            ],
        );
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        assert_eq!(
            upstream.next().headers["authorization"],
            "Bearer native-fixture"
        );
    }
    let now = crate::util::system_now();
    let usage = server
        .state
        .backend
        .usage
        .ledger
        .query(0.0, now, "all", now, None)
        .unwrap();
    assert_eq!(
        usage["totals"]["input_tokens"], 4,
        "replayed image receipt: {usage}"
    );
    assert_eq!(
        usage["totals"]["output_tokens"], 2,
        "replayed image receipt: {usage}"
    );
    server.shutdown().unwrap();
}

#[test]
fn image_26_api_routes_without_a_codex_turn_and_preserves_options() {
    let upstream = ImageUpstream::start(200);
    let (_directory, server) = image_server(&upstream.base_url());
    let mut config = server.state.backend.configuration.read().unwrap().clone();
    config["providers"] = json!([
        {"id":"image-api", "name":"Images", "protocol":"responses", "auth_mode":"api_key",
         "base_url":upstream.base_url(), "api_key":"image-api-fixture", "enabled":true}]);
    let saved = post(
        &server,
        "/api/config",
        &serde_json::to_vec(&config).unwrap(),
        &[&session_header(&server)],
    );
    assert!(saved.starts_with("HTTP/1.1 200"), "{saved}");
    let saved = server.state.backend.configuration.read().unwrap();
    assert_eq!(saved["providers"][0]["api_key"], "");
    assert!(
        saved["providers"][0]["api_key_file"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
    );
    drop(saved);
    for (model, operation, format) in [
        ("gpt-image-2.6-sunburst", "generations", "svg"),
        ("gpt-image-2.6-flare", "generations", "gif"),
        ("gpt-image-2.6-sunburst", "edits", "gif"),
    ] {
        let mut body =
            json!({"model":model,"prompt":"fixture","output_format":format,"complexity":"low"});
        if format == "gif" {
            body["frame_count"] = json!(4);
            body["frame_duration_ms"] = json!(200);
        }
        if operation == "edits" {
            body["images"] = json!([{"image_url":"data:image/png;base64,ZmFrZQ=="}]);
        }
        let wire = post(
            &server,
            &format!("/v1/images/{operation}"),
            &serde_json::to_vec(&body).unwrap(),
            &[
                &session_header(&server),
                "X-EMP-Provider: image-api",
                "Authorization: Bearer caller-secret",
                "chatgpt-account-id: must-not-forward",
            ],
        );
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        let request = upstream.next();
        assert_eq!(request.body, body);
        assert_eq!(request.headers["authorization"], "Bearer image-api-fixture");
        assert!(!request.headers.contains_key("chatgpt-account-id"));
        assert!(request.path.ends_with(&format!("/images/{operation}")));
    }
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["providers"][0]["enabled"] = json!(false);
    for provider in ["unknown-service", "image-api"] {
        let wire = post(
            &server,
            "/v1/images/generations",
            br#"{"model":"gpt-image-2.6-flare","prompt":"fixture"}"#,
            &[
                &session_header(&server),
                &format!("X-EMP-Provider: {provider}"),
            ],
        );
        assert!(wire.starts_with("HTTP/1.1 404"), "{wire}");
    }
    server.shutdown().unwrap();
}

#[test]
fn image_disconnect_cancels_upstream_without_retry() {
    let upstream = ImageUpstream::start(499);
    let (_directory, server) = image_server(&upstream.base_url());
    select_turn(&server, "selected/gpt-6.1-sol", "cancel-turn", false);
    upstream.next();
    let mut socket = TcpStream::connect(server.local_addr()).unwrap();
    let body = r#"{"model":"gpt-image-2","prompt":"fixture"}"#;
    write!(socket,"POST /v1/images/generations HTTP/1.1\r\nHost: {}\r\n{}\r\nx-codex-image-turn-id: cancel-turn\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", server.local_addr(), session_header(&server), body.len()).unwrap();
    assert!(upstream.next().path.ends_with("/images/generations"));
    drop(socket);
    assert_eq!(upstream.next().path, "/cancelled");
    server.shutdown().unwrap();
}
