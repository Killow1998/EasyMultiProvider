use emp_codex::merged_catalog::build_catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[test]
fn external_catalog_keeps_coding_template_without_native_entitlements() {
    let config = json!({
        "providers":[{"id":"demo","protocol":"chat_completions"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"model","context_window":256000,"reasoning_levels":[]}]
    });
    let native = json!({"models":[{
        "slug":"native", "base_instructions":"coding", "model_messages":{"tools":"safe","unknown_future":"private"},
        "available_access_programs":["native entitlement"],"supports_reasoning_summary_parameter":true,
        "multi_agent_version":"orchestrator"
    }]});
    let catalog = build_catalog(&config, &native, &BTreeMap::new(), &BTreeMap::new());
    let external = catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "demo/model")
        .expect("external");
    assert_eq!(external["base_instructions"], "coding");
    assert_eq!(external["display_name"], "[ 256K]  demo/model");
    assert!(external.get("available_access_programs").is_none());
    assert_eq!(external["multi_agent_version"], Value::Null);
    assert_eq!(external["model_messages"], json!({"tools":"safe"}));
    assert_eq!(external["supported_reasoning_levels"], json!([]));
}
