//! Real HTTP/WS turns and image endpoints against synthetic subscription accounts.
use super::support::*;
use super::*;
mod fixture;
mod installed;
mod routing;
use fixture::*;

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
            let mut body = json!({"model":"gpt-image-2.5-sunburst","prompt":"synthetic prompt", "n":2, "background":"transparent", "future":{"preserve":true}});
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
            assert!(wire.contains("x-codex-imagegen-request-id: image-request-fixture\r\n"));
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
                .find(|r| r["model_id"] == format!("{account}/gpt-image-2.5-sunburst"))
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
            .query(0.0, now, "all", now)
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
        assert!(wire.contains("retry-after: 17\r\n"));
        assert!(wire.contains("x-codex-active-limit: image_gen\r\n"));
        assert!(wire.contains("x-image-gen-primary-used-percent: 100\r\n"));
        assert!(wire.contains("x-codex-imagegen-request-id: image-request-fixture\r\n"));
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
        .query(0.0, now, "all", now)
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
