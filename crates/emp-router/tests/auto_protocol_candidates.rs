//! Auto-protocol candidate selection: the order users observe when a
//! provider is configured as `auto` and EMP decides which wire protocol to
//! try first, including when a previously observed protocol is honored.

use emp_core::{RouteSource, deployment_identity, endpoint_fingerprint, resolved_route_from_parts};
use emp_router::protocol_candidates;
use serde_json::json;

fn route(provider: serde_json::Value, model: serde_json::Value) -> emp_core::ResolvedRoute {
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
