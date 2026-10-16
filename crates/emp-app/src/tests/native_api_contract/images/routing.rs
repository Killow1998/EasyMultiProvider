//! Observable account selection, pinning, quota exclusion and failure behavior.
use super::*;

fn configure(server: &ServerHandle, mode: &str, account: &str) {
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["image_routing"] = json!({"mode":mode,"account_id":account});
}

fn image(server: &ServerHandle, turn: &str, operation: &str) -> String {
    post(server, &format!("/v1/images/{operation}"),
        br#"{"model":"gpt-image-2","prompt":"fixture","images":[{"image_url":"data:image/png;base64,ZmFrZQ=="}]}"#,
        &[&session_header(server), &format!("x-codex-image-turn-id: {turn}"),
            "Authorization: Bearer wrong-native-fixture", "chatgpt-account-id: wrong-native-owner"])
}

#[test]
fn image_routing_pins_the_configured_account_without_changing_text_routing() {
    for target in ["other", "@native"] {
        let upstream = ImageUpstream::start(200);
        let (_directory, server) = image_server(&upstream.base_url());
        configure(&server, "account", target);
        select_turn(&server, "selected/gpt-6.1-sol", "pinned", true);
        assert_eq!(
            upstream.next().headers["authorization"],
            "Bearer selected-fixture"
        );
        let expected = if target == "@native" {
            "native"
        } else {
            target
        };
        for operation in ["generations", "edits"] {
            assert!(image(&server, "pinned", operation).starts_with("HTTP/1.1 200"));
            let request = upstream.next();
            assert_eq!(
                request.headers["authorization"],
                format!("Bearer {expected}-fixture")
            );
            assert_eq!(
                request.headers["chatgpt-account-id"],
                format!("{expected}-owner")
            );
            configure(&server, "follow", "");
            // Subsequent inference frames in the same turn must retain the image account.
            select_turn(&server, "selected/gpt-6.1-sol", "pinned", false);
            assert_eq!(
                upstream.next().headers["authorization"],
                "Bearer selected-fixture"
            );
        }
        select_turn(&server, "selected/gpt-6.1-sol", "next", false);
        upstream.next();
        assert!(image(&server, "next", "generations").starts_with("HTTP/1.1 200"));
        assert_eq!(
            upstream.next().headers["authorization"],
            "Bearer selected-fixture"
        );
        server.shutdown().unwrap();
    }
}

#[test]
fn image_routing_automatic_considers_general_and_image_limits_independently_of_review() {
    let upstream = ImageUpstream::start(200);
    let (_directory, server) = image_server(&upstream.base_url());
    configure(&server, "auto", "");
    *server.state.backend.accounts.native_quota.lock().unwrap() =
        Some(json!({"rate_limits":{"primary":{"usedPercent":100}}}));
    {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        config["auto_review_fallback"] = json!(false);
        config["accounts"][0]["quota"] = json!({"rate_limits":{"primary":{"usedPercent":10}},
            "rate_limits_by_limit_id":{"image_gen":{"primary":{"usedPercent":100}}}});
        config["accounts"][1]["quota"] = json!({"rate_limits":{"primary":{"usedPercent":25}},
            "rate_limits_by_limit_id":{"image_gen":{"primary":{"usedPercent":20}}}});
    }
    select_turn(&server, "selected/gpt-6.1-sol", "automatic", false);
    upstream.next();
    assert!(image(&server, "automatic", "generations").starts_with("HTTP/1.1 200"));
    assert_eq!(
        upstream.next().headers["authorization"],
        "Bearer other-fixture"
    );
    {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        config["accounts"][0]["quota"] = json!({"rate_limits":{"primary":{"usedPercent":0}}});
        config["accounts"][1]["quota"] = json!({"rate_limits":{"primary":{"usedPercent":99}}});
    }
    assert!(image(&server, "automatic", "edits").starts_with("HTTP/1.1 200"));
    assert_eq!(
        upstream.next().headers["authorization"],
        "Bearer other-fixture"
    );
    let wire = crate::tests::request(&server, "/api/calls", &[&session_header(&server)]);
    let calls: Value = serde_json::from_slice(response_parts(&wire).1).unwrap();
    assert!(
        calls["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["route"] == "image_edit" && record["account_id"] == "other")
    );
    server.shutdown().unwrap();
}

#[test]
fn image_routing_automatic_learns_limits_without_replaying_requests() {
    for status in [200, 429] {
        let upstream = ImageUpstream::start(status);
        let (_directory, server) = image_server(&upstream.base_url());
        configure(&server, "auto", "");
        *server.state.backend.accounts.native_quota.lock().unwrap() =
            Some(json!({"plan_type":"free"}));
        server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap()["accounts"][0]["quota"] =
            json!({"rate_limits":{"primary":{"usedPercent":10}}});
        for (turn, expected) in [("limited", "selected"), ("fresh", "other")] {
            select_turn(&server, "selected/gpt-6.1-sol", turn, false);
            upstream.next();
            assert!(image(&server, turn, "generations").starts_with(&format!("HTTP/1.1 {status}")));
            assert_eq!(
                upstream.next().headers["authorization"],
                format!("Bearer {expected}-fixture")
            );
            assert!(
                upstream
                    .observed
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
        }
        select_turn(&server, "selected/gpt-6.1-sol", "all-limited", false);
        upstream.next();
        assert!(image(&server, "all-limited", "generations").starts_with("HTTP/1.1 429"));
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
fn image_routing_unavailable_fixed_accounts_never_fall_back() {
    for target in ["other", "@native", "removed"] {
        let upstream = ImageUpstream::start(200);
        let (_directory, server) = image_server(&upstream.base_url());
        configure(&server, "account", target);
        select_turn(&server, "selected/gpt-6.1-sol", "unavailable", false);
        upstream.next();
        if target == "other" {
            server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap()["accounts"][1]["enabled"] = json!(false);
        } else if target == "@native" {
            std::fs::remove_file(&server.state.backend.accounts.native_auth_path).unwrap();
        }
        assert!(image(&server, "unavailable", "edits").starts_with("HTTP/1.1 409"));
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
fn image_routing_keeps_cross_turn_file_references_on_the_image_account() {
    let upstream = ImageUpstream::start(200);
    let (_directory, server) = image_server(&upstream.base_url());
    configure(&server, "account", "other");
    select_turn(&server, "selected/gpt-6.1-sol", "generated", false);
    upstream.next();
    assert!(image(&server, "generated", "generations").starts_with("HTTP/1.1 200"));
    assert_eq!(
        upstream.next().headers["authorization"],
        "Bearer other-fixture"
    );
    configure(&server, "auto", "");
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"][0]["quota"] = json!({"rate_limits":{"primary":{"usedPercent":0}}});
    select_turn(&server, "selected/gpt-6.1-sol", "edit-reference", false);
    upstream.next();
    let body =
        br#"{"model":"gpt-image-2","prompt":"edit","images":[{"file_id":"generation_fixture"}]}"#;
    let send = |turn: &str, body: &[u8]| {
        post(
            &server,
            "/v1/images/edits",
            body,
            &[
                &session_header(&server),
                &format!("x-codex-image-turn-id: {turn}"),
            ],
        )
    };
    assert!(send("edit-reference", body).starts_with("HTTP/1.1 200"));
    assert_eq!(
        upstream.next().headers["authorization"],
        "Bearer other-fixture"
    );
    // An unrecognized server-side reference must not be sent to a guessed account.
    assert!(
        send(
            "edit-reference",
            br#"{"model":"gpt-image-2","images":[{"file_id":"unknown"}]}"#
        )
        .starts_with("HTTP/1.1 409")
    );
    configure(&server, "account", "selected");
    select_turn(&server, "selected/gpt-6.1-sol", "wrong-account", false);
    upstream.next();
    assert!(send("wrong-account", body).starts_with("HTTP/1.1 409"));
    assert!(
        upstream
            .observed
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    server.shutdown().unwrap();
}

#[test]
fn image_routing_automatic_rejects_unusable_accounts_and_expires_stale_windows() {
    for (quota, status) in [
        (json!({"rate_limits":{"primary":{"usedPercent":100}}}), 429),
        (json!({"credits":{"spend_control_reached":true}}), 429),
        (json!({"plan_type":"free"}), 429),
        (
            json!({"rate_limits":{"primary":{"usedPercent":100,"resetsAt":1}}}),
            200,
        ),
    ] {
        let upstream = ImageUpstream::start(200);
        let (_directory, server) = image_server(&upstream.base_url());
        configure(&server, "auto", "");
        *server.state.backend.accounts.native_quota.lock().unwrap() =
            Some(json!({"plan_type":"free"}));
        {
            let mut config = server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap();
            config["accounts"][0]["quota"] = quota;
            config["accounts"][1]["credential_status"] = json!("invalid");
        }
        select_turn(&server, "selected/gpt-6.1-sol", "eligible", false);
        upstream.next();
        assert!(
            image(&server, "eligible", "generations").starts_with(&format!("HTTP/1.1 {status}"))
        );
        if status == 200 {
            assert_eq!(
                upstream.next().headers["authorization"],
                "Bearer selected-fixture"
            );
        } else {
            assert!(
                upstream
                    .observed
                    .recv_timeout(Duration::from_millis(100))
                    .is_err()
            );
        }
        server.shutdown().unwrap();
    }
}

#[test]
fn image_routing_automatic_prefers_known_balance_and_keeps_tie_order() {
    let balance = json!({"rate_limits":{"primary":{"usedPercent":25}}});
    for (native, imported, expected) in [
        (None, Value::Null, "native"),
        (None, balance.clone(), "selected"),
        (Some(balance.clone()), balance.clone(), "native"),
        (
            Some(json!({"plan_type":"free"})),
            balance.clone(),
            "selected",
        ),
    ] {
        let upstream = ImageUpstream::start(200);
        let (_directory, server) = image_server(&upstream.base_url());
        configure(&server, "auto", "");
        *server.state.backend.accounts.native_quota.lock().unwrap() = native;
        {
            let mut config = server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap();
            config["accounts"][0]["quota"] = imported.clone();
            config["accounts"][1]["quota"] = imported;
        }
        select_turn(&server, "other/gpt-6.1-sol", "balance-order", false);
        upstream.next();
        assert!(image(&server, "balance-order", "generations").starts_with("HTTP/1.1 200"));
        assert_eq!(
            upstream.next().headers["authorization"],
            format!("Bearer {expected}-fixture")
        );
        server.shutdown().unwrap();
    }
}
