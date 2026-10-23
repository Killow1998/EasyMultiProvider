//! Exercise routing with real EMP HTTP and synthetic subscription credentials.
use super::auto_review_contract::{make_review_server, write_owned_account_catalog};
use super::*;

pub(super) fn fixture(base_url: &str) -> (TempDir, ServerHandle) {
    let (directory, server, config) = make_review_server(
        base_url,
        &[json!({"id":"selected","prefix":"selected","enabled":true})],
        Some("regular"),
    );
    let models = vec![
        json!({"slug":"gpt-6-luna","supported_in_api":true,"context_window":128000}),
        json!({"slug":"gpt-6.1-sol","supported_in_api":true,"context_window":128000}),
    ];
    std::fs::write(
        config["native_catalog_path"].as_str().unwrap(),
        serde_json::to_vec(&json!({"models":models})).unwrap(),
    )
    .unwrap();
    write_owned_account_catalog(&server, &config["accounts"][0], base_url, models);
    let mut preferences = server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap();
    preferences["memory_model"] = json!("gpt-6-luna");
    preferences["memory_extraction_model"] = json!("gpt-6.1-sol");
    drop(preferences);
    (directory, server)
}

fn prior(server: &ServerHandle, category: &str) {
    let now = crate::util::system_now();
    server.state.backend.usage.ledger.record_call(&json!({
        "request_id":"main-turn","route":"responses","provider_id":"selected","account_id":"selected",
        "model_id":"selected/gpt-6.1-sol","session_id":"conversation","thread_id":"conversation",
        "usage_category":category,"success":true,"status":200,"response_status":"completed"
    }),now-2.0,now-1.0).unwrap();
}

pub(super) fn body() -> Value {
    json!({"model":"gpt-5.6-terra","input":"fixture memory only","stream":false,
        "client_metadata":{"x-codex-turn-metadata":json!({"kind":"memory","thread_id":"conversation"}).to_string()}})
}

#[test]
fn memory_http_follows_subscription_and_uses_native_for_external_and_global_jobs() {
    for (category, global, header, extraction, consolidation, expected_model, expected_token) in [
        (
            "subscription",
            false,
            false,
            "gpt-6.1-sol",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "selected-token",
        ),
        (
            "external",
            false,
            true,
            "gpt-6.1-sol",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "native-secret",
        ),
        (
            "subscription",
            true,
            true,
            "gpt-6.1-sol",
            "gpt-6-luna",
            "gpt-6-luna",
            "native-secret",
        ),
        (
            "external",
            false,
            false,
            "",
            "gpt-6-luna",
            "gpt-6.1-sol",
            "native-secret",
        ),
        (
            "external",
            true,
            true,
            "gpt-6-luna",
            "",
            "gpt-6.1-sol",
            "native-secret",
        ),
    ] {
        let upstream = OneShotUpstream::start(
            json!({"id":"memory-response","object":"response","status":"completed","model":"gpt-6-luna","output":[],"usage":{"input_tokens":10,"output_tokens":1}}),
        );
        let (_directory, server) = fixture(&upstream.base_url());
        {
            let mut preferences = server
                .state
                .backend
                .configuration
                .test_config()
                .lock()
                .unwrap();
            preferences["memory_extraction_model"] = json!(extraction);
            preferences["memory_model"] = json!(consolidation);
        }
        prior(&server, category);
        let auth = session_header(&server);
        let mut headers = vec![auth.as_str()];
        if global {
            headers.push("x-openai-subagent: memory_consolidation");
        } else if header {
            headers.push("x-openai-subagent: memory_extraction");
        }
        let mut request = body();
        request["model"] = json!("gpt-6.1-sol");
        let wire = post(
            &server,
            "/v1/responses",
            &serde_json::to_vec(&request).unwrap(),
            &headers,
        );
        assert!(wire.starts_with("HTTP/1.1 200"), "{wire}");
        let (_, headers, sent) = upstream.observed();
        assert_eq!(headers["authorization"], format!("Bearer {expected_token}"));
        assert_eq!(sent["model"], expected_model);
        assert_eq!(
            server
                .state
                .backend
                .usage
                .ledger
                .latest_conversation_call("conversation")
                .unwrap()
                .unwrap()["request_id"],
            "main-turn"
        );
        server.shutdown().unwrap();
    }
}

#[test]
fn memory_quota_pause_is_visible_and_does_not_redirect_to_another_account() {
    let (_directory, server) = fixture("http://127.0.0.1:9/v1");
    prior(&server, "subscription");
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"][0]["quota"] = json!({"rate_limits":{"primary":{"usedPercent":100}}});
    let wire = post(
        &server,
        "/v1/responses",
        &serde_json::to_vec(&body()).unwrap(),
        &[&session_header(&server)],
    );
    assert!(wire.starts_with("HTTP/1.1 503"), "{wire}");
    assert!(wire.contains("Memory generation paused"), "{wire}");
    assert_eq!(
        server.state.backend.memory_status.snapshot()["reason"],
        "quota_exhausted"
    );
    assert_eq!(
        server.state.backend.memory_status.snapshot()["kind"],
        "memory_extraction"
    );
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"][0]["quota"] =
        json!({"rate_limits":{"primary":{"usedPercent":100,"resetsAt":1}}});
    let prepared =
        crate::services::memory_routing::prepare(&server.state, body(), &Default::default())
            .unwrap();
    assert_eq!(prepared.route.provider.value()["account"]["id"], "selected");
    server.shutdown().unwrap();
}

#[test]
fn memory_router_ignores_prompt_words_and_rejects_conflicting_source_metadata() {
    let (_directory, server) = fixture("http://127.0.0.1:9/v1");
    let normal =
        json!({"model":"selected/gpt-6.1-sol","input":"memory consolidation thread_title"});
    let prepared =
        crate::services::memory_routing::prepare(&server.state, normal, &Default::default())
            .unwrap();
    assert_eq!(prepared.route.requested_model, "selected/gpt-6.1-sol");
    assert!(matches!(
        crate::services::memory_routing::prepare(
            &server.state,
            body(),
            &BTreeMap::from([("thread-id".into(), "another".into())])
        ),
        Err(
            crate::services::request_preparation::RequestPreparationError::MemoryUnavailable(
                "source_conflict"
            )
        )
    ));
    server.shutdown().unwrap();
}
