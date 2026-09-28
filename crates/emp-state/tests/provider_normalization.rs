use emp_state::normalize_provider;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-provider-normalization.json"
    ))
    .expect("valid provider fixture")
}

#[test]
fn provider_normalization_matches_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid providers") {
        assert_eq!(
            normalize_provider(&case["input"]).expect("valid provider"),
            case["expected"]
        );
    }
    for case in fixture["invalid"].as_array().expect("invalid providers") {
        assert_eq!(
            normalize_provider(&case["input"])
                .expect_err("invalid provider")
                .to_string(),
            case["error"].as_str().expect("provider error")
        );
    }
}
