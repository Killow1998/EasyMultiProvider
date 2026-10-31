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
        // Previously official values are refreshed from the bundled registry.
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
fn proxy_models_gain_published_modalities_without_assuming_deployment_limits() {
    let models = vec![
        json!({"upstream_id": "gpt-5.6-sol"}),
        json!({"upstream_id":"anthropic/claude-opus-5-5"}),
        json!({"upstream_id":"mercury-3"}),
        json!({"upstream_id":"deployment-alias"}),
    ];
    let enriched = enrich_discovered_models(
        provider("https://unknown.example/v1").as_object().unwrap(),
        models,
    );
    assert_eq!(enriched[0]["input_modalities"], json!(["text", "image"]));
    assert_eq!(enriched[1]["input_modalities"], json!(["text", "image"]));
    assert_eq!(enriched[2]["input_modalities"], json!(["text"]));
    for model in &enriched[..3] {
        assert_eq!(
            model["capability_sources"]["input_modalities"]["source"],
            "official"
        );
        assert!(model.get("context_window").is_none());
        assert!(model.get("reasoning_levels").is_none());
        assert!(model.get("supported_protocols").is_none());
    }
    assert_eq!(enriched[3], json!({"upstream_id":"deployment-alias"}));
}

#[test]
fn explicit_proxy_restrictions_override_published_model_modalities() {
    for source in ["manual", "observed", "advertised"] {
        let enriched = enrich_discovered_models(
            provider("https://proxy.example/v1").as_object().unwrap(),
            vec![
                json!({"upstream_id":"gpt-5.6-sol", "input_modalities":["text"],
                "capability_sources":{"input_modalities":{"source":source}}}),
            ],
        );
        assert_eq!(enriched[0]["input_modalities"], json!(["text"]));
        assert_eq!(
            enriched[0]["capability_sources"]["input_modalities"]["source"],
            source
        );
    }
}

#[test]
fn every_published_model_and_alias_preserves_its_declared_modalities() {
    let registry: Value =
        serde_json::from_str(include_str!("../../emp-state/data/official_models.json")).unwrap();
    let mut expected = Vec::new();
    let mut models = Vec::new();
    let mut identities = std::collections::HashSet::new();
    for model in registry["models"].as_array().unwrap() {
        let id = model["model_id"].as_str().unwrap();
        assert!(model["input_modalities"].is_array(), "{id}");
        for candidate in std::iter::once(id).chain(
            model["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|alias| alias.as_str().unwrap()),
        ) {
            assert!(
                identities.insert(candidate),
                "ambiguous model ID: {candidate}"
            );
            models.push(json!({"upstream_id":candidate}));
            expected.push((candidate, &model["input_modalities"]));
        }
    }
    let enriched = enrich_discovered_models(
        provider("https://relay.example.invalid/v1")
            .as_object()
            .unwrap(),
        models,
    );
    for (actual, (id, modalities)) in enriched.iter().zip(expected) {
        assert_eq!(&actual["input_modalities"], modalities, "{id}");
    }
}

#[test]
fn official_model_redirects_do_not_relabel_retained_aggregator_models() {
    for (endpoint, expected) in [
        ("https://api.deepseek.com/v1", json!(["text", "image"])),
        ("https://openrouter.ai/api/v1", json!(["text"])),
        ("https://relay.example.invalid/v1", json!(["text"])),
    ] {
        let enriched = enrich_discovered_models(
            provider(endpoint).as_object().unwrap(),
            vec![json!({"upstream_id":"deepseek-v4-flash"})],
        );
        assert_eq!(enriched[0]["input_modalities"], expected, "{endpoint}");
        assert_eq!(enriched[0]["upstream_id"], "deepseek-v4-flash");
    }
    let advertised = enrich_discovered_models(
        provider("https://api.deepseek.com/v1").as_object().unwrap(),
        vec![
            json!({"upstream_id":"deepseek-v4-flash", "input_modalities":["text"],
                   "capability_sources":{"input_modalities":{"source":"advertised"}}}),
        ],
    );
    assert_eq!(advertised[0]["input_modalities"], json!(["text"]));
}
