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

#[test]
fn aggregates_remain_distinct_from_their_source_family_and_each_other() {
    let models = json!([
        {"id":"model","family_id":"model","default_display_name":"Original","source_type":"native"},
        {"id":"account/model","family_id":"model","default_display_name":"Other · Original","source_type":"account","source_id":"account"},
        {"id":"auto/first","family_id":"model","default_display_name":"autoOriginal","display_name":"[95K] auto⭐","source_type":"aggregate"},
        {"id":"auto/second","family_id":"model","default_display_name":"spareOriginal","display_name":"[95K] spare⭐","source_type":"aggregate"},
    ]);
    let config = json!({
        "accounts":[{"id":"account","name":"Other"}],
        "catalog_family_presentations":{"model":{"catalog_alias":"⭐"}},
        "catalog_presentations":{"auto/second":{"catalog_alias":"备用"}},
    });
    let actual = catalog_families(&config, models.as_array().expect("models"));
    assert_eq!(actual.len(), 3);
    assert_eq!(actual[0]["id"], "model");
    assert_eq!(actual[0]["display_name"], "⭐");
    assert_eq!(actual[0]["routes"].as_array().unwrap().len(), 2);
    assert_eq!(actual[1]["id"], "auto/first");
    assert_eq!(actual[1]["display_name"], "auto⭐");
    assert_eq!(actual[1]["routes"].as_array().unwrap().len(), 1);
    assert_eq!(actual[2]["id"], "auto/second");
    assert_eq!(actual[2]["display_name"], "备用");
}
