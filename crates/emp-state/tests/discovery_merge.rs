use emp_state::{discovery_merge::merge_selected_models, normalize_configuration};
use serde_json::{Value, json};

const NOW: &str = "2026-09-22T00:00:00+00:00";

fn fixture() -> Value {
    json!({
        "config": {
            "native_catalog_path": "fixture-native.json",
            "providers": [{"id":"demo", "base_url":"https://example.invalid/v1"}],
            "models": [
                {"id":"demo/manual", "provider":"demo", "upstream_id":"manual", "context_window":77777,
                 "input_modalities":["text","image"], "reasoning_levels":["high"], "visibility":"hide",
                 "capability_sources":{"context_window":{"source":"manual"},"input_modalities":{"source":"manual"},"reasoning_levels":{"source":"unknown"}}},
                {"id":"demo/disabled", "provider":"demo", "upstream_id":"disabled", "enabled":false},
                {"id":"demo/hidden", "provider":"demo", "upstream_id":"hidden"},
                {"id":"demo/unavailable", "provider":"demo", "upstream_id":"unavailable"}
            ]
        },
        "discovered": [
            {"upstream_id":"manual", "context_window":128000, "input_modalities":["text"],
             "reasoning_levels":[], "created_at":123, "family_id":"family-a",
             "capability_sources":{"context_window":{"source":"advertised"},"input_modalities":{"source":"official"},"reasoning_levels":{"source":"unknown"}}},
            {"upstream_id":"disabled", "context_window":80000},
            {"upstream_id":"hidden"},
            {"upstream_id":"new", "display_name":"New", "description":"from discovery", "context_window":256000,
             "input_modalities":["text","image","audio"], "reasoning_levels":["high","low"], "supports_reasoning":true,
             "supports_reasoning_summaries":false, "supported_protocols":["responses","chat_completions"],
             "capabilities":{"streaming":true,"structured_output":false},
             "capability_sources":{"supports_reasoning":{"source":"advertised"},"streaming":{"source":"official"}}}
        ],
        "selections":[["manual","disabled","new"],[],["absent"],false,[1],["new","new"]]
    })
}

#[test]
fn selected_discovery_merge_respects_manual_edits_and_validates_selections() {
    let fixture = fixture();
    let config = normalize_configuration(Some(&fixture["config"])).expect("normalize config");
    let discovered = fixture["discovered"].as_array().expect("discovery results");
    let results = fixture["selections"].as_array().expect("selections").iter().map(|selected| {
        match merge_selected_models(&config, "demo", discovered, selected, NOW) {
            Ok(result) => json!({"config":result.config, "available":result.available, "added":result.added, "hidden":result.hidden}),
            Err(error) => json!({"error_type":error.python_type(), "error":error.to_string()}),
        }
    }).collect::<Vec<_>>();
    assert_eq!(results[0]["hidden"], 1);
    assert_eq!(results[0]["added"], 1);
    let models = results[0]["config"]["models"].as_array().expect("models");
    assert_eq!(models[0]["context_window"], 77777);
    assert_eq!(models[0]["input_modalities"], json!(["text", "image"]));
    assert_eq!(models[0]["reasoning_levels"], json!([]));
    assert_eq!(models[1]["enabled"], false);
    assert_eq!(models[3]["enabled"], true);
    assert_eq!(results[2]["error_type"], "ConfigError");
}
