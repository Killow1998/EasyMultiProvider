use emp_state::normalize_model;
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-model-normalization.json"
    ))
    .expect("valid model normalization fixture")
}

#[test]
fn model_normalization_matches_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid models") {
        let actual = normalize_model(&case["input"]).expect("valid model");
        assert_eq!(
            actual,
            case["expected"],
            "case: {}",
            case["name"].as_str().expect("case name")
        );
        assert_eq!(
            actual.as_object().map(|fields| fields.len()),
            Some(26),
            "case: {}",
            case["name"].as_str().expect("case name")
        );
    }
    for case in fixture["invalid"].as_array().expect("invalid models") {
        let error = normalize_model(&case["input"]).expect_err("invalid model");
        assert_eq!(
            error.python_type(),
            case["error_type"].as_str().expect("model error type"),
            "case: {}",
            case["name"].as_str().expect("case name")
        );
        assert_eq!(
            error.to_string(),
            case["error"].as_str().expect("model error"),
            "case: {}",
            case["name"].as_str().expect("case name")
        );
    }
}

#[test]
fn output_token_limit_presence_is_not_truthiness() {
    let base = json!({"id": "provider-a/semantics", "provider": "provider-a"});
    let shadowing = [
        Value::Null,
        Value::Bool(false),
        Value::from(""),
        Value::Array(Vec::new()),
        Value::Object(serde_json::Map::new()),
    ];
    for raw_output_limit in shadowing {
        let mut raw = base.clone();
        raw["output_limit"] = raw_output_limit.clone();
        raw["output_token_limit"] = json!(91_000);
        assert_eq!(
            normalize_model(&raw).expect("normalizable model")["output_limit"],
            json!(0),
            "output_limit {raw_output_limit} must suppress the alias"
        );
    }
}
