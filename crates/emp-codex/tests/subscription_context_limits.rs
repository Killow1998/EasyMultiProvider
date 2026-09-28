use emp_codex::subscription_contexts::validate_subscription_contexts;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn validation_reports_native_source_before_account_sources() {
    let catalog = json!({"models":[]});
    let config = json!({
        "native_model_context_windows":{"native":1},
        "accounts":[
            {"id":"first","model_context_windows":{"first":1}},
            {"id":"second","model_context_windows":{"second":1}},
        ],
    });
    let error = validate_subscription_contexts(&config, None, &catalog, &BTreeMap::new())
        .expect_err("native invalid settings are rejected first");
    let rendered = error.to_string();
    assert!(rendered.contains("native"), "{rendered}");
    assert!(
        rendered.contains("subscription catalog limit"),
        "{rendered}"
    );

    let valid_native = json!({
        "native_model_context_windows":{},
        "accounts":[
            {"id":"first","model_context_windows":{"first":1}},
            {"id":"second","model_context_windows":{"second":1}},
        ],
    });
    let error = validate_subscription_contexts(&valid_native, None, &catalog, &BTreeMap::new())
        .expect_err("account sources follow native configuration order");
    let rendered = error.to_string();
    assert!(rendered.contains("first"), "{rendered}");
    assert!(
        rendered.contains("subscription catalog limit"),
        "{rendered}"
    );
}

#[test]
fn changed_settings_follow_current_limits_and_unchanged_settings_remain_allowed() {
    let native = json!({"models":[
        {"slug":"model","context_window":272000,"max_context_window":700000},
        {"slug":"invalid","context_window":272000,"max_context_window":"1000000"},
    ]});
    let previous = json!({
        "native_model_context_windows":{"model":1000001},
        "accounts":[{"id":"account","model_context_windows":{"model":700001}}],
    });
    let config = json!({
        "native_model_context_windows":{"model":700000},
        "accounts":[{"id":"account","model_context_windows":{"model":700001}}],
    });
    let catalogs = BTreeMap::from([("account".to_owned(), native.clone())]);
    assert!(validate_subscription_contexts(&config, Some(&previous), &native, &catalogs).is_ok());

    let unchanged = json!({
        "native_model_context_windows":{"model":1000001},
        "accounts":[{"id":"account","model_context_windows":{"model":700001}}],
    });
    assert!(
        validate_subscription_contexts(&unchanged, Some(&previous), &native, &catalogs).is_ok()
    );

    let over_limit = json!({
        "native_model_context_windows":{"model":700001},
        "accounts":[{"id":"account","model_context_windows":{}}],
    });
    let error = validate_subscription_contexts(&over_limit, None, &native, &catalogs)
        .expect_err("unknown or over-limit settings must be rejected");
    let rendered = error.to_string();
    assert!(rendered.contains("model"), "{rendered}");
    assert!(
        rendered.contains("subscription catalog limit"),
        "{rendered}"
    );
}
