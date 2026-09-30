use emp_state::{discovery_merge::merge_selected_models, normalize_configuration};
use serde_json::{Value, json};

fn configuration() -> Value {
    json!({
        "providers":[{"id":"claude","protocol":"anthropic_messages",
            "execution_backend":"claude_cli","auth_mode":"api_key",
            "base_url":"https://cpa.example.invalid/v1"}],
        "models":(["claude-sonnet-5-5", "claude-opus-5-5"].map(|id| json!({
            "id":format!("claude/{id}"), "provider":"claude", "upstream_id":id,
            "reasoning_levels":[], "supports_reasoning":null, "input_modalities":["text"],
            "capability_sources":{"reasoning_levels":{"source":"unknown"},
                "input_modalities":{"source":"unknown"}}
        })))
    })
}

#[test]
fn existing_cpa_and_local_claude_models_gain_missing_capabilities_on_load() {
    let mut raw = configuration();
    for local in [false, true] {
        if local {
            raw["providers"][0]["auth_mode"] = json!("claude_login");
            raw["providers"][0]["base_url"] = json!("");
        }
        let loaded = normalize_configuration(Some(&raw)).unwrap();
        for model in loaded["models"].as_array().unwrap() {
            assert_eq!(
                model["reasoning_levels"],
                json!(["low", "medium", "high", "xhigh", "max"])
            );
            assert_eq!(model["supports_reasoning"], true);
            assert_eq!(model["input_modalities"], json!(["text", "image"]));
            assert_eq!(
                model["capability_sources"]["input_modalities"]["source"],
                "official"
            );
            assert_eq!(model["context_window"], 0, "do not guess deployment limits");
        }
        assert_eq!(normalize_configuration(Some(&loaded)).unwrap(), loaded);
    }
}

#[test]
fn explicit_restrictions_survive_reload_and_sparse_model_refresh() {
    for source in ["manual", "advertised", "observed"] {
        let mut raw = configuration();
        raw["models"][0]["supports_reasoning"] = json!(false);
        raw["models"][0]["capability_sources"] = json!({
            "reasoning_levels":{"source":source}, "supports_reasoning":{"source":source},
            "input_modalities":{"source":source}
        });
        let loaded = normalize_configuration(Some(&raw)).unwrap();
        assert_eq!(loaded["models"][0]["reasoning_levels"], json!([]));
        assert_eq!(loaded["models"][0]["supports_reasoning"], false);
        assert_eq!(loaded["models"][0]["input_modalities"], json!(["text"]));
        {
            let advertised = emp_state::official_registry::enrich_discovered_models(
                loaded["providers"][0].as_object().unwrap(),
                vec![json!({"upstream_id":"claude-sonnet-5-5"})],
            );
            let result = merge_selected_models(
                &loaded,
                "claude",
                &advertised,
                &json!(["claude-sonnet-5-5"]),
                "2026-09-30T00:00:00Z",
            )
            .unwrap();
            assert_eq!(result.config["models"][0]["reasoning_levels"], json!([]));
            assert_eq!(
                result.config["models"][0]["input_modalities"],
                json!(["text"])
            );
        }
    }
}

#[test]
fn arbitrary_models_and_http_proxies_do_not_inherit_claude_defaults() {
    let mut raw = configuration();
    raw["providers"][0]["execution_backend"] = json!("http");
    let loaded = normalize_configuration(Some(&raw)).unwrap();
    assert_eq!(loaded["models"][0]["reasoning_levels"], json!([]));
    raw["providers"][0]["execution_backend"] = json!("claude_cli");
    raw["models"][0]["upstream_id"] = json!("custom-model");
    let loaded = normalize_configuration(Some(&raw)).unwrap();
    assert_eq!(loaded["models"][0]["reasoning_levels"], json!([]));
    assert_eq!(loaded["models"][0]["input_modalities"], json!(["text"]));
}
