//! Auto-protocol candidate selection: the order users observe when a
//! provider is configured as `auto` and EMP decides which wire protocol to
//! try first, including when a previously observed protocol is honored.

use emp_config::{
    config::{LearnedProtocol, ProtocolSave, remember_learned_protocol},
    normalize_configuration,
    official_registry::bundled_snapshot,
};
use emp_domain::{
    Protocol, RouteSource, deployment_identity, endpoint_fingerprint,
    resolve_route_without_catalog, resolved_route_from_parts,
};
use emp_router::{observed_protocol, protocol_candidates};
use serde_json::json;

fn route(provider: serde_json::Value, model: serde_json::Value) -> emp_domain::ResolvedRoute {
    resolved_route_from_parts(
        model["id"].as_str().expect("model id"),
        provider.as_object().expect("provider object").clone(),
        model.as_object().expect("model object").clone(),
        RouteSource::ExplicitModel,
    )
    .expect("auto route")
}

fn candidates(provider: serde_json::Value, model: serde_json::Value) -> Vec<String> {
    protocol_candidates(&route(provider, model))
        .into_iter()
        .map(|protocol| protocol.as_config_str().to_owned())
        .collect()
}

fn base_provider() -> serde_json::Value {
    json!({
        "id": "demo", "base_url": "https://external.example/v1",
        "protocol": "auto", "auth_mode": "api_key", "api_key": "test-key"
    })
}

fn base_model(id: &str) -> serde_json::Value {
    json!({"id": format!("{id}/model"), "provider": id, "upstream_id": "upstream-model"})
}

#[test]
fn generic_endpoints_try_chat_first_and_responses_suffixed_endpoints_flip_the_order() {
    assert_eq!(
        candidates(base_provider(), base_model("chat")),
        ["chat_completions", "responses"]
    );
    let provider = json!({
        "id": "responses", "base_url": "https://external.example/v1/responses/",
        "protocol": "auto", "auth_mode": "api_key", "api_key": "test-key"
    });
    assert_eq!(
        candidates(provider, base_model("responses")),
        ["responses", "chat_completions"]
    );
}

#[test]
fn anthropic_auth_mode_skips_openai_protocols_entirely() {
    let provider = json!({
        "id": "anthropic", "base_url": "https://external.example/v1",
        "protocol": "auto", "auth_mode": "anthropic_api_key", "api_key": "test-key"
    });
    assert_eq!(
        candidates(provider, base_model("anthropic")),
        ["anthropic_messages"]
    );
}

#[test]
fn claude_cli_auto_route_selects_anthropic_messages_for_its_native_relay() {
    let provider = json!({
        "id":"claude-cli", "base_url":"https://external.example/v1",
        "protocol":"auto", "auth_mode":"api_key", "api_key":"test-key",
        "execution_backend":"claude_cli"
    });
    assert_eq!(
        candidates(provider, base_model("claude-cli")),
        ["anthropic_messages"]
    );
}

#[test]
fn a_matching_observation_is_tried_first_but_stale_observations_are_ignored() {
    let observed_provider = base_provider();
    let mut observed_model = base_model("observed");
    observed_model["resolved_protocol"] = json!("responses");
    observed_model["protocol_observation"] = json!({
        "endpoint_fingerprint": endpoint_fingerprint(Some("https://external.example/v1")),
        "deployment_identity": deployment_identity(
            observed_provider.as_object().unwrap(),
            observed_model.as_object().unwrap(),
        ),
        "upstream_model": "upstream-model"
    });
    assert_eq!(
        candidates(observed_provider, observed_model),
        ["responses", "chat_completions"]
    );

    // A saved observation whose fingerprint no longer matches the endpoint
    // must not influence the order.
    let stale_provider = json!({
        "id": "stale", "base_url": "https://external.example/v1",
        "protocol": "auto", "auth_mode": "api_key", "api_key": "test-key",
        "resolved_protocol": "responses",
        "protocol_observation": {
            "endpoint_fingerprint": format!("sha256:{}", "0".repeat(64)),
            "deployment_identity": "default",
            "upstream_model": "upstream-model"
        }
    });
    assert_eq!(
        candidates(stale_provider, base_model("stale")),
        ["chat_completions", "responses"]
    );
}

#[test]
fn saving_an_observation_preserves_whitespace_in_the_resolved_model_identity() {
    let config = normalize_configuration(Some(&json!({
        "providers": [base_provider()],
        "models": [
            {"id": "demo/spaced", "provider": "demo", "upstream_id": "demo/ m"},
            {"id": "demo/plain", "provider": "demo", "upstream_id": "m"}
        ]
    })))
    .unwrap();
    let spaced = resolve_route_without_catalog(&config, "demo/spaced").unwrap();
    assert_eq!(spaced.upstream_model, " m");
    let candidate = LearnedProtocol::from_route(
        &spaced,
        Protocol::Responses,
        "2026-09-22T10:00:00+00:00".to_owned(),
    )
    .unwrap();
    let ProtocolSave::Updated(saved) =
        remember_learned_protocol(&config, &candidate, bundled_snapshot).unwrap()
    else {
        panic!("the observation must save");
    };
    let spaced = resolve_route_without_catalog(&saved, "demo/spaced").unwrap();
    let plain = resolve_route_without_catalog(&saved, "demo/plain").unwrap();
    assert_eq!(observed_protocol(&spaced), Some(Protocol::Responses));
    assert_eq!(observed_protocol(&plain), None);
}

#[test]
fn a_repeated_provider_prefix_does_not_match_a_legacy_observation_for_another_model() {
    let mut provider = base_provider();
    provider["resolved_protocol"] = json!("responses");
    provider["protocol_observation"] = json!({
        "source": "observed",
        "endpoint_fingerprint": endpoint_fingerprint(provider["base_url"].as_str()),
        "deployment_identity": "default",
        "upstream_model": "demo/m"
    });
    let config = normalize_configuration(Some(&json!({
        "providers": [provider],
        "models": [
            {"id": "demo/original", "provider": "demo", "upstream_id": "demo/m"},
            {"id": "demo/repeated", "provider": "demo", "upstream_id": "demo/demo/m"}
        ]
    })))
    .unwrap();
    let original = resolve_route_without_catalog(&config, "demo/original").unwrap();
    let repeated = resolve_route_without_catalog(&config, "demo/repeated").unwrap();
    assert_eq!(original.upstream_model, "m");
    assert_eq!(repeated.upstream_model, "demo/m");
    assert_eq!(observed_protocol(&original), None);
    assert_eq!(observed_protocol(&repeated), None);
}
