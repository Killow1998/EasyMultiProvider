//! Bundled provider registry: how discovered models get official context
//! limits and capability flags, and which provider a base URL belongs to.
//! This drives the capability facts shown in the web UI.

use emp_router::official_registry::{enrich_discovered_models, identify_provider};
use serde_json::{Value, json};

fn provider(base_url: &str) -> Value {
    json!({"base_url": base_url})
}

#[test]
fn official_base_urls_map_to_a_single_provider_key() {
    assert_eq!(
        identify_provider(provider("https://api.openai.com/v1/").as_object().unwrap()),
        Some("openai".to_owned())
    );
    // URLs carrying credentials or a query string are never recognized, so
    // they never pick up official capability facts.
    assert_eq!(
        identify_provider(
            provider("https://user:secret@api.openai.com/v1")
                .as_object()
                .unwrap()
        ),
        None
    );
    assert_eq!(
        identify_provider(
            provider("https://api.openai.com/v1?version=1")
                .as_object()
                .unwrap()
        ),
        None
    );
    assert_eq!(
        identify_provider(
            provider("https://open.bigmodel.cn/api/paas/v4")
                .as_object()
                .unwrap()
        ),
        Some("zhipu_glm".to_owned())
    );
    assert_eq!(
        identify_provider(provider("https://unknown.example/v1").as_object().unwrap()),
        None
    );
}

#[test]
fn an_explicit_official_provider_is_honored_only_for_matching_endpoints() {
    assert_eq!(
        identify_provider(
            json!({"base_url": "https://api.openai.com/v1", "official_provider": "openai"})
                .as_object()
                .unwrap()
        ),
        Some("openai".to_owned())
    );
    assert_eq!(
        identify_provider(
            json!({"base_url": "https://proxy.example/v1", "official_provider": "openai"})
                .as_object()
                .unwrap()
        ),
        None,
        "an explicit override cannot claim official facts for a proxy URL"
    );
}

#[test]
fn enrichment_fills_only_fields_the_user_has_not_overridden() {
    let provider = provider("https://api.openai.com/v1/");
    let models = vec![
        // Everything unknown -> filled from the registry.
        json!({"upstream_id": "gpt-5.6-sol"}),
        // Advertised values stay; unknown stays fillable.
        json!({
            "upstream_id": "gpt-5.6",
            "input_modalities": ["text"],
            "capabilities": {"streaming": false},
            "capability_sources": {
                "input_modalities": {"source": "advertised"},
                "streaming": {"source": "unknown"}
            }
        }),
        // An official source is authoritative and is not overwritten again.
        json!({
            "upstream_id": "gpt-5.6-sol",
            "context_window": 1,
            "capability_sources": {
                "context_window": {
                    "source": "official",
                    "confidence": 0.95,
                    "observed_at": "2026-01-01"
                }
            }
        }),
        // A null value is treated as unknown and gets filled.
        json!({"upstream_id": "future-model", "context_window": null}),
        // Unknown models pass through untouched.
        json!({"upstream_id": "unlisted-model"}),
    ];
    let enriched = enrich_discovered_models(provider.as_object().unwrap(), models);

    assert_eq!(enriched[0]["context_window"], json!(1_050_000));
    assert_eq!(
        enriched[0]["capability_sources"]["context_window"]["source"],
        "official"
    );

    assert_eq!(enriched[1]["input_modalities"], json!(["text"]));
    assert_eq!(
        enriched[1]["capability_sources"]["input_modalities"]["source"],
        "advertised"
    );
    assert_eq!(enriched[1]["capabilities"]["streaming"], true);
    assert_eq!(
        enriched[1]["capability_sources"]["streaming"]["source"],
        "official"
    );

    assert_eq!(enriched[2]["context_window"], 1_050_000);
    assert_eq!(enriched[3]["context_window"], Value::Null);
    assert_eq!(enriched[4], json!({"upstream_id": "unlisted-model"}));
}

#[test]
fn models_from_unrecognized_providers_pass_through_untouched() {
    let models = vec![json!({"upstream_id": "gpt-5.6-sol"})];
    let enriched = enrich_discovered_models(
        provider("https://unknown.example/v1").as_object().unwrap(),
        models,
    );
    assert_eq!(enriched[0], json!({"upstream_id": "gpt-5.6-sol"}));
    assert!(enriched[0].get("capability_sources").is_none());
}
