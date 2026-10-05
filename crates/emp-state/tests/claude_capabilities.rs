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

fn configuration_46() -> Value {
    let mut raw = configuration();
    raw["models"] =
        json!([
        ("claude-opus-4-6", 1_000_000),
        ("claude-sonnet-4-6", 200_000)
    ].map(|(id, context)| json!({
        "id":format!("claude/{id}"), "provider":"claude", "upstream_id":id,
        "supports_reasoning":null, "reasoning_levels":[], "reasoning_control":"",
        "context_window":context, "output_limit":8192, "input_modalities":["text"],
        "capability_sources":{"reasoning_levels":{"source":"unknown"},
            "supports_reasoning":{"source":"unknown"}, "reasoning_control":{"source":"unknown"}}
    })));
    raw
}

#[test]
fn saved_claude_46_models_recover_missing_efforts_without_changing_route_or_limits() {
    for local in [false, true] {
        let mut raw = configuration_46();
        if local {
            raw["providers"][0]["auth_mode"] = json!("claude_login");
            raw["providers"][0]["base_url"] = json!("");
        }
        let loaded = normalize_configuration(Some(&raw)).unwrap();
        for (before, model) in raw["models"]
            .as_array()
            .unwrap()
            .iter()
            .zip(loaded["models"].as_array().unwrap())
        {
            assert_eq!(
                model["reasoning_levels"],
                json!(["low", "medium", "high", "max"])
            );
            assert_eq!(model["supports_reasoning"], true);
            assert_eq!(model["reasoning_control"], "effort");
            for field in [
                "id",
                "provider",
                "upstream_id",
                "context_window",
                "output_limit",
                "input_modalities",
            ] {
                assert_eq!(model[field], before[field], "preserve {field}");
            }
            for field in [
                "reasoning_levels",
                "supports_reasoning",
                "reasoning_control",
            ] {
                assert_eq!(model["capability_sources"][field]["source"], "official");
            }
        }
        assert_eq!(normalize_configuration(Some(&loaded)).unwrap(), loaded);
    }
}

#[test]
fn claude_46_restrictions_survive_load_and_discovery_refresh() {
    for source in ["manual", "advertised", "observed"] {
        for levels in [json!([]), json!(["low"])] {
            let mut raw = configuration_46();
            for model in raw["models"].as_array_mut().unwrap() {
                model["reasoning_levels"] = levels.clone();
                model["supports_reasoning"] = json!(!levels.as_array().unwrap().is_empty());
                model["capability_sources"]["reasoning_levels"] = json!({"source":source});
                model["capability_sources"]["supports_reasoning"] = json!({"source":source});
            }
            let loaded = normalize_configuration(Some(&raw)).unwrap();
            let discovered = emp_state::official_registry::enrich_discovered_models(
                loaded["providers"][0].as_object().unwrap(),
                vec![
                    json!({"upstream_id":"claude-opus-4-6"}),
                    json!({"upstream_id":"claude-sonnet-4-6"}),
                ],
            );
            let refreshed = merge_selected_models(
                &loaded,
                "claude",
                &discovered,
                &json!(["claude-opus-4-6", "claude-sonnet-4-6"]),
                "2026-10-05T19:00:00+08:00",
            )
            .unwrap();
            for config in [&loaded, &refreshed.config] {
                for model in config["models"].as_array().unwrap() {
                    assert_eq!(model["reasoning_levels"], levels);
                    assert_eq!(
                        model["supports_reasoning"],
                        !levels.as_array().unwrap().is_empty()
                    );
                    assert_eq!(
                        model["capability_sources"]["reasoning_levels"]["source"],
                        source
                    );
                }
            }
        }
    }
}
