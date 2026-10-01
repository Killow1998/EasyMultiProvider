use emp_state::normalize_configuration;
use serde_json::{Value, json};

/// Frozen data users' disks may already contain. Expected values come from the
/// fixture; failure cases assert the error kind plus one stable key fragment.
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-config-normalization.json"
    ))
    .expect("valid full configuration fixture")
}

fn error_fragment(case: &Value) -> &'static str {
    match case["name"].as_str().expect("case name") {
        "raw-not-object" => "must be an object",
        "host-not-loopback" => "host",
        "port-out-of-range" => "port",
        "url-validates-before-port-range" => "codex_base_url",
        // Conversion failures name the offending value type; that stable
        // fragment is the Rust contract, not the interpreter sentence.
        "port-invalid-integer-text" | "port-null-type-error" => "int(",
        "accounts-null-not-iterable" => "'NoneType' object is not iterable",
        "account-error-type-preserved" => "account.id",
        "duplicate-account-id-before-providers" => "account ids",
        "duplicate-account-prefix" => "account prefixes",
        "provider-error-before-model" => "provider.base_url",
        "model-error-before-provider-duplicate" => "model.id",
        "duplicate-provider-id" => "provider ids",
        "account-prefix-provider-conflict-sorted" => "prefixes conflict",
        "duplicate-model-id" => "model ids",
        "unknown-model-providers-sorted" => "unknown providers",
        "tail-hidden-model-error-after-relations" => "native_hidden_models",
        "tail-presentation-error-before-runtime" => "catalog_presentations",
        "runtime-source-error" => "codex_runtime_sources",
        other => panic!("unmapped error case {other}"),
    }
}

#[test]
fn configuration_normalization_matches_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid configurations") {
        let actual = normalize_configuration(Some(&case["input"])).expect("valid configuration");
        let mut python_compatible = actual.clone();
        python_compatible
            .as_object_mut()
            .expect("normalized object")
            .remove("catalog_show_context");
        // These three defaults intentionally changed after the Python baseline.
        for key in ["auto_enable_on_start", "auto_review_fallback"] {
            python_compatible.as_object_mut().unwrap().remove(key);
        }
        let mut expected = case["expected"].clone();
        if !case["input"]["subscription_search"]["enabled"].is_boolean() {
            expected["subscription_search"]["enabled"] = json!(true);
        }
        assert_eq!(python_compatible, expected, "case: {}", case["name"]);
    }
    for case in fixture["invalid"]
        .as_array()
        .expect("invalid configurations")
    {
        let error =
            normalize_configuration(Some(&case["input"])).expect_err("invalid configuration");
        let rendered = error.to_string();
        let fragment = error_fragment(case);
        assert!(
            rendered.contains(fragment),
            "case {}: {rendered:?} must name {fragment:?}",
            case["name"]
        );
    }
}

#[test]
fn normalization_rejects_non_object_and_bad_types_with_stable_fragments() {
    // Non-object and wrong-type inputs fail closed with the offending field
    // named; None falls back to defaults rather than failing.
    let error = normalize_configuration(Some(&json!([]))).expect_err("array is not an object");
    assert_eq!(error.python_type(), "ConfigError");
    assert!(error.to_string().contains("object"));

    let error = normalize_configuration(Some(&json!({"port": "bad"}))).expect_err("string port");
    assert_eq!(error.python_type(), "ValueError");
    assert!(error.to_string().contains("port") || error.to_string().contains("int("));

    let defaults = normalize_configuration(None).expect("defaults");
    assert_eq!(defaults["port"], 4200);
    assert_eq!(defaults["catalog_show_context"], true);
    for input in [None, Some(json!({}))] {
        let defaults = normalize_configuration(input.as_ref()).unwrap();
        assert_eq!(defaults["auto_enable_on_start"], true);
        assert_eq!(defaults["auto_review_fallback"], true);
        assert_eq!(defaults["subscription_search"]["enabled"], true);
    }
    let disabled = normalize_configuration(Some(&json!({
        "auto_enable_on_start":false, "auto_review_fallback":false,
        "subscription_search":{"enabled":false}
    })))
    .unwrap();
    assert_eq!(disabled["auto_enable_on_start"], false);
    assert_eq!(disabled["auto_review_fallback"], false);
    assert_eq!(disabled["subscription_search"]["enabled"], false);

    let hidden = normalize_configuration(Some(&json!({"catalog_show_context": false})))
        .expect("explicit display preference");
    assert_eq!(hidden["catalog_show_context"], false);
    let error = normalize_configuration(Some(&json!({"catalog_show_context": "false"})))
        .expect_err("context display preference must be boolean");
    assert!(error.to_string().contains("catalog_show_context"));
}

#[test]
fn application_preferences_survive_web_save_and_reject_non_booleans() {
    let current = normalize_configuration(Some(&json!({
        "auto_enable_on_start": true, "auto_review_fallback": false
    })))
    .unwrap();
    let public =
        emp_state::public_configuration_with_file_status(&current, &Default::default(), |_| false)
            .unwrap();
    let saved = emp_state::merge_web_update(&current, &public).unwrap();
    assert_eq!(saved["auto_enable_on_start"], true);
    assert_eq!(saved["auto_review_fallback"], false);
    for key in ["auto_enable_on_start", "auto_review_fallback"] {
        assert!(normalize_configuration(Some(&json!({key: "false"}))).is_err());
    }
}
