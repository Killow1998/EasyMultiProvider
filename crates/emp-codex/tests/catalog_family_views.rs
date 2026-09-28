use emp_codex::management_views::catalog_families;
use serde_json::json;

#[test]
fn family_controls_prefer_native_limits_and_intersect_summary_support() {
    let models = json!([
        {"id":"provider/model","family_id":"model","default_display_name":"External","context_window":128000,"source_type":"provider","source_id":"provider","supports_reasoning_summaries":false},
        {"id":"account/model","family_id":"model","default_display_name":"Account","context_window":256000,"source_type":"account","source_id":"account","supports_reasoning_summaries":true},
        {"id":"model","family_id":"model","default_display_name":"Native","context_window":512000,"source_type":"native","source_id":"","supports_reasoning_summaries":true},
    ]);
    let config = json!({"catalog_family_presentations":{"model":{"catalog_alias":"Daily","show_context":false,"reasoning_summary":"show"}}});
    let actual = catalog_families(&config, models.as_array().expect("models"));
    assert_eq!(actual[0]["default_display_name"], "Native");
    assert_eq!(actual[0]["context_window"], 512000);
    assert_eq!(actual[0]["supports_reasoning_summaries"], false);
    assert_eq!(actual[0]["display_name"], "Daily");
}
