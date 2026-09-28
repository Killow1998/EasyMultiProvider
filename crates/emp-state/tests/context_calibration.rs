use emp_state::normalize_context_calibrations;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-context-calibrations.json"
    ))
    .expect("valid context calibration fixture")
}

#[test]
fn context_calibrations_match_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid cases") {
        assert_eq!(
            normalize_context_calibrations(Some(&case["input"])).expect("valid calibration"),
            case["expected"],
            "case: {}",
            case["name"].as_str().expect("case name"),
        );
    }
    for case in fixture["invalid"].as_array().expect("invalid cases") {
        assert_eq!(
            normalize_context_calibrations(Some(&case["input"]))
                .expect_err("invalid calibration")
                .to_string(),
            case["error"].as_str().expect("error string"),
            "case: {}",
            case["name"].as_str().expect("case name"),
        );
        assert_eq!(case["error_type"], "ConfigError");
    }
}

#[test]
fn null_context_calibrations_normalize_to_empty() {
    assert_eq!(
        normalize_context_calibrations(None).expect("null normalization"),
        Value::Array(Vec::new()),
    );
}
