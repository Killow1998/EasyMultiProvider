//! Routing results users see in Codex and the web UI: model selection,
//! account prefixes, forward fallback, and the dialect each provider gets.

use emp_core::{Dialect, RouteSource, resolve_route_without_catalog};
use serde_json::{Value, json};

fn config() -> Value {
    json!({
        "codex_base_url": "https://codex.example/backend-api/codex",
        "providers": [
            {"id": "demo", "base_url": "https://api.demo.example/v1", "protocol": "chat_completions", "enabled": true},
            {"id": "ext", "base_url": "https://responses.demo.example/v1", "protocol": "responses", "auth_mode": "api_key", "enabled": true},
            {"id": "native", "base_url": "https://codex.example/backend-api/codex", "protocol": "responses", "auth_mode": "forward", "enabled": true},
            {"id": "down", "base_url": "https://down.example/v1", "protocol": "responses", "enabled": false}
        ],
        "models": [
            {"id": "work/gpt-5", "provider": "demo", "upstream_id": "gpt-5", "enabled": true},
            {"id": "lab/local-model", "provider": "down", "enabled": true},
            {"id": "ext/answers", "provider": "ext", "enabled": true}
        ],
        "accounts": [
            {"id": "team", "prefix": "team", "enabled": true}
        ]
    })
}

#[test]
fn explicit_models_route_to_their_provider_with_upstream_ids() {
    let route = resolve_route_without_catalog(&config(), "work/gpt-5").expect("explicit route");
    assert_eq!(route.source, RouteSource::ExplicitModel);
    assert_eq!(route.upstream_model, "gpt-5");
    assert_eq!(route.provider_id, "demo");
    assert_eq!(route.dialect, Dialect::ChatCompletions);

    let responses_route =
        resolve_route_without_catalog(&config(), "ext/answers").expect("responses route");
    assert_eq!(responses_route.dialect, Dialect::PortableResponses);
}

#[test]
fn account_prefixes_route_to_native_accounts_and_reject_unknown_prefixes() {
    let config = json!({
        "codex_base_url": "https://codex.example/backend-api/codex",
        "accounts": [{"id": "team", "prefix": "team", "enabled": true}]
    });
    let route = resolve_route_without_catalog(&config, "team/gpt-5").expect("account subscription");
    assert_eq!(route.source, RouteSource::SubscriptionAccount);
    assert_eq!(route.upstream_model, "gpt-5");
    assert_eq!(route.dialect, Dialect::CodexNative);

    // A stale or foreign prefix must not silently route anywhere.
    let error =
        resolve_route_without_catalog(&config, "stale/gpt-5").expect_err("unknown account prefix");
    assert_eq!(error.status(), 404);

    // A disabled account fails closed as unavailable, not unknown.
    let disabled = json!({
        "accounts": [{"id": "team", "prefix": "team", "enabled": false}]
    });
    let error =
        resolve_route_without_catalog(&disabled, "team/gpt-5").expect_err("disabled account");
    assert_eq!(error.status(), 503);
}

#[test]
fn unqualified_names_fall_back_to_a_single_forward_provider_or_fail() {
    // One enabled forward provider takes unqualified names.
    let single = json!({
        "providers": [{"id": "native", "base_url": "https://other.example/v1",
            "protocol": "responses", "auth_mode": "forward", "enabled": true}]
    });
    let route = resolve_route_without_catalog(&single, "gpt-5").expect("forward fallback");
    assert_eq!(route.source, RouteSource::ForwardProvider);
    assert_eq!(route.upstream_model, "gpt-5");

    // Qualified names never hit the fallback: they must exist explicitly.
    let error =
        resolve_route_without_catalog(&single, "unknown/model").expect_err("qualified unknown");
    assert_eq!(error.status(), 404);
}

#[test]
fn endpoints_are_fingerprinted_without_credentials_query_or_fragments() {
    use emp_core::endpoint_fingerprint;
    use emp_core::normalize_endpoint;

    assert_eq!(
        normalize_endpoint(Some(
            "HTTPS://Api.Example.com:443/v1/../v1/responses?token=x#frag"
        )),
        "https://api.example.com/v1/responses"
    );
    // Default ports collapse; non-default ports stay.
    assert_eq!(
        normalize_endpoint(Some("http://api.example.com:80/v1")),
        "http://api.example.com/v1"
    );
    assert_eq!(
        normalize_endpoint(Some("http://api.example.com:8080/v1")),
        "http://api.example.com:8080/v1"
    );
    // Non-HTTP endpoints and garbage canonicalize to empty.
    assert_eq!(normalize_endpoint(Some("gopher://example.test")), "");
    assert_eq!(normalize_endpoint(Some("   ")), "");
    assert_eq!(normalize_endpoint(None), "");

    // The fingerprint hides the endpoint itself but is stable and distinct.
    let plain = endpoint_fingerprint(Some("https://api.example.com/v1"));
    let with_secret = endpoint_fingerprint(Some("https://user:token@api.example.com/v1?x=1"));
    assert!(plain.starts_with("sha256:"));
    assert_eq!(plain, with_secret);
    assert_ne!(
        plain,
        endpoint_fingerprint(Some("https://api.example.com/v2"))
    );
}

#[test]
fn dialect_and_deployment_identity_come_from_provider_metadata() {
    use emp_core::classify_dialect;
    use emp_core::deployment_identity;

    let anthropic = serde_json::from_value::<serde_json::Map<String, Value>>(json!({
        "protocol": "anthropic_messages"
    }))
    .expect("provider");
    assert_eq!(classify_dialect(&anthropic), Dialect::AnthropicMessages);
    let native = serde_json::from_value::<serde_json::Map<String, Value>>(json!({
        "protocol": "responses", "auth_mode": "forward"
    }))
    .expect("provider");
    assert_eq!(classify_dialect(&native), Dialect::CodexNative);

    let provider =
        serde_json::from_value::<serde_json::Map<String, Value>>(json!({})).expect("provider");
    let model = serde_json::from_value::<serde_json::Map<String, Value>>(json!({
        "deployment_identity": "team/primary"
    }))
    .expect("model");
    assert_eq!(deployment_identity(&provider, &model), "team/primary");
    let hostile = serde_json::from_value::<serde_json::Map<String, Value>>(json!({
        "deployment_identity": "bad identity with spaces"
    }))
    .expect("model");
    assert_eq!(deployment_identity(&provider, &hostile), "default");
}
