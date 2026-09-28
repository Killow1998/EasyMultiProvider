use emp_state::normalize_configuration;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-config-normalization.json"
    ))
    .expect("valid full configuration fixture")
}

#[test]
fn configuration_normalization_matches_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid configurations") {
        let actual = normalize_configuration(Some(&case["input"])).expect("valid configuration");
        assert_eq!(actual, case["expected"], "case: {}", case["name"]);
        assert_eq!(actual.as_object().map(|object| object.len()), Some(15));
    }
    for case in fixture["invalid"]
        .as_array()
        .expect("invalid configurations")
    {
        let error =
            normalize_configuration(Some(&case["input"])).expect_err("invalid configuration");
        assert_eq!(
            error.python_type(),
            case["error_type"],
            "case: {}",
            case["name"]
        );
        assert_eq!(error.to_string(), case["error"], "case: {}", case["name"]);
    }
}
