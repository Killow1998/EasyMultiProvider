use emp_state::normalize_model_capability_sources;
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-model-capability-sources.json"
    ))
    .expect("valid model capability-source fixture")
}

fn case_values<'a>(fixture: &'a Value, case: &'a Value) -> &'a Value {
    case.get("values").unwrap_or(&fixture["values"])
}

fn explicit_fields<'a>(fixture: &'a Value, case: &'a Value) -> Vec<&'a str> {
    case.get("explicit_fields")
        .unwrap_or(&fixture["explicit_fields"])
        .as_array()
        .expect("explicit fields")
        .iter()
        .map(|value| value.as_str().expect("explicit field name"))
        .collect()
}

#[test]
fn model_capability_sources_match_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid cases") {
        let explicit = explicit_fields(&fixture, case);
        assert_eq!(
            normalize_model_capability_sources(
                case.get("input"),
                case_values(&fixture, case),
                Some(&explicit)
            )
            .expect("valid capability sources"),
            case["expected"]
        );
    }
    for case in fixture["invalid"].as_array().expect("invalid cases") {
        let explicit = explicit_fields(&fixture, case);
        assert_eq!(
            normalize_model_capability_sources(
                case.get("input"),
                case_values(&fixture, case),
                Some(&explicit)
            )
            .expect_err("invalid capability sources")
            .to_string(),
            case["error"].as_str().expect("capability-source error")
        );
    }
}
