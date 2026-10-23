use emp_codex::merged_catalog::build_catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[test]
fn ultra_override_preserves_the_harness_and_tracks_supported_subscription_efforts() {
    let model = json!({"slug":"gpt-ultra","display_name":"Ultra fixture","visibility":"list",
        "default_reasoning_level":"high","multi_agent_reasoning_effort":"xhigh","multi_agent_version":"v2",
        "model_messages":{"multi_agent":{"mode":"official"}},
        "supported_reasoning_levels":[{"effort":"low"},{"effort":"high"},{"effort":"xhigh"},{"effort":"ultra"}]});
    let native = json!({"models":[model]});
    let accounts = BTreeMap::from([("sub".into(), native.clone())]);
    let mut config = json!({"accounts":[{"id":"sub","prefix":"sub","auth_file":"fixture"}],
        "providers":[{"id":"external","protocol":"responses"}],
        "models":[{"id":"external/gpt-ultra","provider":"external","upstream_id":"gpt-ultra","reasoning_levels":["low","high","ultra"]}],
        "catalog_presentations":{"gpt-ultra":{"catalog_alias":"Native alias"},"sub/gpt-ultra":{"catalog_alias":"Imported alias"}},
        "ultra_reasoning_efforts":{"gpt-ultra":"low"}});
    let build =
        |config: &Value, native: &Value| build_catalog(config, native, &accounts, &BTreeMap::new());
    let mut baseline_config = config.clone();
    baseline_config["ultra_reasoning_efforts"] = json!({});
    let baseline = build(&baseline_config, &native);
    let overridden = build(&config, &native);
    let mut expected = baseline.clone();
    for entry in expected["models"].as_array_mut().unwrap() {
        if entry["slug"] != "external/gpt-ultra" {
            entry["multi_agent_reasoning_effort"] = json!("low");
        }
    }
    assert_eq!(
        overridden, expected,
        "Only the subscription Ultra mapping changes"
    );
    assert_ne!(
        emp_state::catalog_etag(&overridden),
        emp_state::catalog_etag(&baseline)
    );
    let views =
        emp_codex::management_views::model_views(&config, &native, &accounts, &BTreeMap::new());
    for entry in views["catalog_models"].as_array().unwrap() {
        assert_eq!(
            entry["ultra_default_effort"],
            if entry["source_type"] == "provider" {
                Value::Null
            } else {
                json!("xhigh")
            }
        );
        assert_eq!(
            entry["ultra_reasoning_levels"],
            if entry["source_type"] == "provider" {
                json!([])
            } else {
                json!(["low", "high", "xhigh"])
            }
        );
    }
    for value in ["max", "ultra", "unknown"] {
        config["ultra_reasoning_efforts"]["gpt-ultra"] = json!(value);
        assert_eq!(
            build(&config, &native),
            baseline,
            "Unsupported override {value} uses official metadata"
        );
    }
    config["ultra_reasoning_efforts"] = json!({});
    assert_eq!(build(&config, &native), baseline);
    config["ultra_reasoning_efforts"]["gpt-ultra"] = json!("low");
    let mut refreshed = native.clone();
    refreshed["models"][0]["supported_reasoning_levels"] =
        json!([{"effort":"high"},{"effort":"ultra"}]);
    refreshed["models"][0]["multi_agent_reasoning_effort"] = json!("high");
    let catalog = build(&config, &refreshed);
    assert_eq!(
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["slug"] == "gpt-ultra")
            .unwrap()["multi_agent_reasoning_effort"],
        "high"
    );
    refreshed["models"][0]["supported_reasoning_levels"] =
        json!([{"effort":"low"},{"effort":"high"}]);
    let catalog = build(&config, &refreshed);
    assert_eq!(
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["slug"] == "gpt-ultra")
            .unwrap()["multi_agent_reasoning_effort"],
        "high"
    );
}

#[test]
fn claude_model_picker_uses_cli_defaults_with_model_specific_effort_levels() {
    for (id, default, count) in [
        ("claude-haiku-5-5", "medium", 5),
        ("claude-sonnet-5-5", "medium", 5),
        ("claude-opus-5-5", "medium", 5),
        ("claude-fable-5-1", "high", 5),
        ("claude-opus-5", "high", 5),
        ("claude-sonnet-5", "high", 5),
        ("claude-fable-5", "high", 5),
        ("claude-opus-4-8", "high", 5),
        ("claude-opus-4-7", "xhigh", 5),
        ("claude-opus-4-6", "high", 4),
        ("claude-sonnet-4-6", "high", 4),
    ] {
        let raw = json!({"providers":[{"id":"claude","protocol":"anthropic_messages",
            "execution_backend":"claude_cli","auth_mode":"api_key","base_url":"https://example.invalid"}],
            "models":[{"id":format!("claude/{id}"),"provider":"claude","upstream_id":id}]});
        let config = emp_state::normalize_configuration(Some(&raw)).unwrap();
        let catalog = build_catalog(
            &config,
            &json!({"models":[]}),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        let model = &catalog["models"][0];
        assert_eq!(model["default_reasoning_level"], default, "{id}");
        assert_eq!(
            model["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            count,
            "{id}"
        );
    }
}

#[test]
fn claude_default_stays_within_restricted_efforts_and_does_not_change_http_models() {
    let mut raw = json!({"providers":[{"id":"claude","execution_backend":"claude_cli"}],
        "models":[{"id":"claude/model","provider":"claude","upstream_id":"claude-haiku-5-5",
            "reasoning_levels":["low","high"]}]});
    let catalog = build_catalog(
        &raw,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(catalog["models"][0]["default_reasoning_level"], "low");
    raw["providers"][0]["execution_backend"] = json!("http");
    raw["models"][0]["reasoning_levels"] = json!(["low", "medium", "high", "xhigh", "max"]);
    let catalog = build_catalog(
        &raw,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    assert_eq!(catalog["models"][0]["default_reasoning_level"], "high");
}

#[test]
fn external_catalog_keeps_coding_template_without_native_entitlements() {
    let config = json!({
        "providers":[{"id":"demo","protocol":"chat_completions"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"model","context_window":256000,"reasoning_levels":[]}]
    });
    let native = json!({"models":[{
        "slug":"native", "base_instructions":"coding", "model_messages":{"tools":"safe","unknown_future":"private"},
        "available_access_programs":["native entitlement"],"supports_reasoning_summary_parameter":true,
        "multi_agent_version":"orchestrator", "truncation_policy":{"mode":"tokens","limit":12000}
    }]});
    let catalog = build_catalog(&config, &native, &BTreeMap::new(), &BTreeMap::new());
    let external = catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "demo/model")
        .expect("external");
    assert_eq!(external["base_instructions"], "coding");
    // Codex keeps 95% of the window for input, so 256000 shows as 243K.
    assert_eq!(external["display_name"], "[ 243K]  demo/model");
    assert!(external.get("available_access_programs").is_none());
    assert_eq!(external["multi_agent_version"], Value::Null);
    assert_eq!(external["model_messages"], json!({"tools":"safe"}));
    assert_eq!(external["supported_reasoning_levels"], json!([]));
    assert_eq!(
        external["truncation_policy"],
        native["models"][0]["truncation_policy"]
    );
}

#[test]
fn external_catalog_without_a_native_template_has_codex_required_truncation_policy() {
    let config = json!({
        "providers":[{"id":"demo","execution_backend":"claude_cli"}],
        "models":[{"id":"demo/sonnet","provider":"demo"}]
    });
    for native in [json!({"models":[]}), json!({"models":[{"slug":"native"}]})] {
        let catalog = build_catalog(&config, &native, &BTreeMap::new(), &BTreeMap::new());
        let model = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["slug"] == "demo/sonnet")
            .unwrap();
        // Codex model_info_from_slug uses 10,000 bytes for unknown models.
        assert_eq!(
            model["truncation_policy"],
            json!({"mode":"bytes","limit":10000})
        );
    }
}

#[test]
fn claude_cli_catalog_keeps_known_images_but_not_unproven_image_detail_or_other_modalities() {
    let config = json!({
        "providers":[{"id":"demo","protocol":"anthropic_messages","execution_backend":"claude_cli"}],
        "models":[{
            "id":"demo/model",
            "provider":"demo",
            "input_modalities":["text","image","audio","video"],
            "supports_image_detail_original":true
        }]
    });
    let native = json!({"models":[]});

    let claude_catalog = build_catalog(&config, &native, &BTreeMap::new(), &BTreeMap::new());
    let claude_model = claude_catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "demo/model")
        .expect("Claude CLI model");
    assert_eq!(claude_model["input_modalities"], json!(["text", "image"]));
    assert_eq!(claude_model["supports_image_detail_original"], false);

    let mut http_config = config.clone();
    http_config["providers"][0]["execution_backend"] = json!("http");
    let http_catalog = build_catalog(&http_config, &native, &BTreeMap::new(), &BTreeMap::new());
    let http_model = http_catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "demo/model")
        .expect("HTTP model");
    assert_eq!(http_model["input_modalities"], json!(["text", "image"]));
    assert_eq!(http_model["supports_image_detail_original"], true);
    assert_eq!(
        config["models"][0]["input_modalities"],
        json!(["text", "image", "audio", "video"])
    );
    assert_eq!(config["models"][0]["supports_image_detail_original"], true);
}

#[test]
fn claude_cli_catalog_does_not_invent_image_support_when_not_advertised() {
    let config = json!({
        "providers":[{"id":"demo","protocol":"anthropic_messages","execution_backend":"claude_cli"}],
        "models":[{"id":"demo/text-only","provider":"demo","input_modalities":["text","audio","video"]}]
    });
    let catalog = build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let model = catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "demo/text-only")
        .expect("text-only model");

    assert_eq!(model["input_modalities"], json!(["text"]));
    assert_eq!(
        config["models"][0]["input_modalities"],
        json!(["text", "audio", "video"])
    );
}

#[test]
fn claude_login_configuration_generates_a_credential_free_codex_route() {
    let config = emp_state::normalize_configuration(Some(&json!({
        "providers":[{
            "id":"claude-local",
            "protocol":"anthropic_messages",
            "execution_backend":"claude_cli",
            "auth_mode":"claude_login"
        }],
        "models":[{
            "id":"claude-local/sonnet",
            "provider":"claude-local",
            "upstream_id":"sonnet",
            "input_modalities":["text"]
        }]
    })))
    .expect("local login configuration");
    let catalog = build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let model = catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "claude-local/sonnet")
        .expect("Codex picker route");

    assert_eq!(model["input_modalities"], json!(["text"]));
    assert!(model.get("api_key").is_none());
    assert!(model.get("api_key_file").is_none());
}

#[test]
fn global_context_preference_changes_only_labels_for_every_catalog_source() {
    let config = json!({
        "accounts":[{"id":"subscription","name":"Account","prefix":"sub","auth_file":"credentials"}],
        "providers":[{"id":"demo","protocol":"chat_completions"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"model","context_window":6400,"display_name":"Provider model","description":"Provider description"}],
        "catalog_presentations":{
            "native-model":{"catalog_alias":"Native alias","show_context":true},
            "sub/chat":{"catalog_alias":"Account alias","show_context":false},
            "demo/model":{"catalog_alias":"Provider alias","show_context":false,"reasoning_summary":"show"}
        }
    });
    let native = json!({"models":[{
        "slug":"native-model","display_name":"Native model","description":"Native description",
        "context_window":1600,"visibility":"list","supported_in_api":true
    }]});
    let account_catalogs = BTreeMap::from([(
        "subscription".to_owned(),
        json!({"models":[{
            "slug":"chat","display_name":"Chat model","description":"Account model",
            "context_window":3200,"visibility":"list","supported_in_api":true
        }]}),
    )]);

    let mut hidden_config = config.clone();
    hidden_config["catalog_show_context"] = json!(false);
    let hidden = build_catalog(&hidden_config, &native, &account_catalogs, &BTreeMap::new());
    let default_visible = build_catalog(&config, &native, &account_catalogs, &BTreeMap::new());

    for route in ["native-model", "sub/chat", "demo/model"] {
        let hidden_model = hidden["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["slug"] == route)
            .unwrap_or_else(|| panic!("hidden model {route}"));
        let visible_model = default_visible["models"]
            .as_array()
            .expect("models")
            .iter()
            .find(|model| model["slug"] == route)
            .unwrap_or_else(|| panic!("visible model {route}"));
        assert!(
            !hidden_model["display_name"]
                .as_str()
                .unwrap()
                .starts_with('[')
        );
        assert!(
            !hidden_model["description"]
                .as_str()
                .unwrap()
                .contains("Context ")
        );
        assert!(
            visible_model["display_name"]
                .as_str()
                .unwrap()
                .starts_with('[')
        );
        assert!(
            visible_model["description"]
                .as_str()
                .unwrap()
                .contains("Context ")
        );
        assert!(
            hidden_model["context_window"]
                .as_u64()
                .is_some_and(|value| value > 0)
        );
        assert_eq!(
            hidden_model["default_reasoning_summary"],
            visible_model["default_reasoning_summary"]
        );
        for field in ["context_window", "max_context_window"] {
            assert_eq!(hidden_model[field], visible_model[field], "{route} {field}");
        }
    }

    for (route, alias) in [
        ("native-model", "Native alias"),
        ("sub/chat", "Account alias"),
        ("demo/model", "Provider alias"),
    ] {
        assert!(
            hidden["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["slug"] == route)
                .unwrap()["display_name"]
                .as_str()
                .unwrap()
                .contains(alias)
        );
        assert!(
            default_visible["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["slug"] == route)
                .unwrap()["display_name"]
                .as_str()
                .unwrap()
                .contains(alias)
        );
    }
}

#[test]
fn claude_picker_receives_all_efforts_and_images_after_loading_sparse_saved_models() {
    let config = emp_state::normalize_configuration(Some(&json!({
        "providers":[{"id":"cpa","base_url":"https://cpa.example.invalid/v1",
            "execution_backend":"claude_cli","protocol":"anthropic_messages"}],
        "models":[{"id":"cpa/claude-sonnet-5-5","provider":"cpa","upstream_id":"claude-sonnet-5-5"}]
    })))
    .unwrap();
    let catalog = build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    let model = catalog["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "cpa/claude-sonnet-5-5")
        .unwrap();
    assert_eq!(model["input_modalities"], json!(["text", "image"]));
    let efforts: Vec<_> = model["supported_reasoning_levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|level| level["effort"].as_str().unwrap())
        .collect();
    assert_eq!(efforts, ["low", "medium", "high", "xhigh", "max"]);
}

#[test]
fn claude_46_picker_recovers_legal_efforts_and_default_from_unknown_saved_capabilities() {
    let config = emp_state::normalize_configuration(Some(&json!({
        "providers":[{"id":"cpa","base_url":"https://cpa.example.invalid/v1",
            "execution_backend":"claude_cli","protocol":"anthropic_messages"}],
        "models":(["claude-opus-4-6", "claude-sonnet-4-6"].map(|id| json!({
            "id":format!("cpa/{id}"), "provider":"cpa", "upstream_id":id,
            "supports_reasoning":null, "reasoning_levels":[], "reasoning_control":"",
            "capability_sources":{"reasoning_levels":{"source":"unknown"}}
        })))
    })))
    .unwrap();
    let catalog = build_catalog(
        &config,
        &json!({"models":[]}),
        &BTreeMap::new(),
        &BTreeMap::new(),
    );
    for model in catalog["models"].as_array().unwrap() {
        let levels: Vec<_> = model["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["effort"].as_str().unwrap())
            .collect();
        assert_eq!(levels, ["low", "medium", "high", "max"]);
        assert_eq!(model["default_reasoning_level"], "high");
        assert!(levels.contains(&model["default_reasoning_level"].as_str().unwrap()));
    }
    assert_eq!(catalog["models"].as_array().unwrap().len(), 2);
}
