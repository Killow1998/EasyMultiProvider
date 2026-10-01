use emp_state::{
    codex_input_modalities, input_modalities_known, input_modalities_metadata_source,
    normalize_input_modalities, normalize_output_modalities, normalize_reasoning_levels,
    normalize_supported_protocols, output_modalities_known, output_modalities_metadata_source,
    supported_protocols_known,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-capability-model-values.json"
    ))
    .expect("valid capability model-value fixture")
}

#[test]
fn capability_model_values_match_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["modalities"]["cases"]
        .as_array()
        .expect("modality cases")
    {
        let input = case.get("input");
        for (actual, known, source) in [
            (
                normalize_input_modalities(input),
                input_modalities_known(input),
                input_modalities_metadata_source(input),
            ),
            (
                normalize_output_modalities(input),
                output_modalities_known(input),
                output_modalities_metadata_source(input),
            ),
        ] {
            assert_eq!(json!(actual), case["expected"], "{case}");
            assert_eq!(known, case["known"], "{case}");
            assert_eq!(
                source,
                case["metadata_source"].as_str().expect("source"),
                "{case}"
            );
        }
        assert_eq!(
            json!(codex_input_modalities(input)),
            case["codex"],
            "{case}"
        );
    }
    for case in fixture["protocols"].as_array().expect("protocol cases") {
        let normalized = normalize_supported_protocols(case.get("input"));
        assert_eq!(json!(normalized), case["expected"], "{case}");
        assert_eq!(supported_protocols_known(case.get("input")), case["known"]);
    }
    for case in fixture["reasoning_levels"]
        .as_array()
        .expect("reasoning cases")
    {
        assert_eq!(
            json!(normalize_reasoning_levels(case.get("input"))),
            case["expected"],
            "{case}"
        );
    }
}

#[test]
fn modality_bounds_and_grammar_use_utf8_bytes() {
    let fixture = fixture();
    let bounds = &fixture["bounds"];
    let max_modalities = bounds["max_modalities"].as_u64().expect("modality limit") as usize;
    let max_bytes = bounds["max_modality_id_bytes"]
        .as_u64()
        .expect("modality byte limit") as usize;

    let at_limit = vec!["x"; max_modalities];
    let over_limit = vec!["x"; max_modalities + 1];
    assert!(input_modalities_known(Some(&json!(at_limit))));
    assert!(!input_modalities_known(Some(&json!(over_limit))));

    let at_byte_limit = "x".repeat(max_bytes);
    let over_byte_limit = "x".repeat(max_bytes + 1);
    assert!(input_modalities_known(Some(&json!([at_byte_limit]))));
    assert!(!input_modalities_known(Some(&json!([over_byte_limit]))));
}
