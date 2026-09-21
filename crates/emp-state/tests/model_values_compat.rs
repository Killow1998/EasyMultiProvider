use emp_state::{
    codex_input_modalities, input_modalities_known, input_modalities_metadata_source,
    normalize_input_modalities, normalize_output_modalities, normalize_reasoning_levels,
    normalize_supported_protocols, output_modalities_known, output_modalities_metadata_source,
    supported_protocols_known,
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-capability-model-values.json"
    ))
    .expect("valid capability model-value fixture")
}

#[test]
fn capability_model_values_match_frozen_python_fixture() {
    let fixture = fixture();
    for case in fixture["modalities"]["cases"]
        .as_array()
        .expect("modality cases")
    {
        let input = case.get("input");
        assert_eq!(
            normalize_input_modalities(input),
            case["expected"]
                .as_array()
                .expect("input modality expected")
                .iter()
                .map(|value| value.as_str().expect("string value").to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            normalize_output_modalities(input),
            case["expected"]
                .as_array()
                .expect("output modality expected")
                .iter()
                .map(|value| value.as_str().expect("string value").to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(input_modalities_known(input), case["known"]);
        assert_eq!(output_modalities_known(input), case["known"]);
        assert_eq!(
            input_modalities_metadata_source(input),
            case["metadata_source"].as_str().expect("input source")
        );
        assert_eq!(
            output_modalities_metadata_source(input),
            case["metadata_source"].as_str().expect("output source")
        );
        assert_eq!(
            codex_input_modalities(input),
            case["codex"]
                .as_array()
                .expect("Codex modalities")
                .iter()
                .map(|value| value.as_str().expect("string value").to_owned())
                .collect::<Vec<_>>()
        );
    }
    for case in fixture["protocols"].as_array().expect("protocol cases") {
        let normalized = normalize_supported_protocols(case.get("input"));
        assert_eq!(
            normalized,
            case["expected"]
                .as_array()
                .expect("protocol expected")
                .iter()
                .map(|value| value.as_str().expect("string value").to_owned())
                .collect::<Vec<_>>()
        );
        assert_eq!(supported_protocols_known(case.get("input")), case["known"]);
    }
    for case in fixture["reasoning_levels"]
        .as_array()
        .expect("reasoning cases")
    {
        assert_eq!(
            normalize_reasoning_levels(case.get("input")),
            case["expected"]
                .as_array()
                .expect("reasoning expected")
                .iter()
                .map(|value| value.as_str().expect("string value").to_owned())
                .collect::<Vec<_>>()
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

#[test]
fn capability_values_match_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.capabilities import (
    codex_input_modalities,
    input_modalities_known,
    input_modalities_metadata_source,
    normalize_input_modalities,
    normalize_output_modalities,
    normalize_reasoning_levels,
    normalize_supported_protocols,
    output_modalities_known,
    output_modalities_metadata_source,
    supported_protocols_known,
)
fixture = json.load(sys.stdin)
json.dump({
    "modalities": [{
        "expected": normalize_input_modalities(case["input"]),
        "output_expected": normalize_output_modalities(case["input"]),
        "known": input_modalities_known(case["input"]),
        "output_known": output_modalities_known(case["input"]),
        "metadata_source": input_modalities_metadata_source(case["input"]),
        "output_metadata_source": output_modalities_metadata_source(case["input"]),
        "codex": codex_input_modalities(case["input"]),
    } for case in fixture["modalities"]["cases"]],
    "protocols": [{
        "expected": normalize_supported_protocols(case["input"]),
        "known": supported_protocols_known(case["input"]),
    } for case in fixture["protocols"]],
    "reasoning_levels": [{
        "expected": normalize_reasoning_levels(case["input"]),
    } for case in fixture["reasoning_levels"]],
}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let rust = json!({
        "modalities": fixture["modalities"]["cases"]
            .as_array()
            .expect("modality cases")
            .iter()
            .map(|case| {
                let input = case.get("input");
                json!({
                    "expected": normalize_input_modalities(input),
                    "output_expected": normalize_output_modalities(input),
                    "known": input_modalities_known(input),
                    "output_known": output_modalities_known(input),
                    "metadata_source": input_modalities_metadata_source(input),
                    "output_metadata_source": output_modalities_metadata_source(input),
                    "codex": codex_input_modalities(input),
                })
            })
            .collect::<Vec<_>>(),
        "protocols": fixture["protocols"]
            .as_array()
            .expect("protocol cases")
            .iter()
            .map(|case| {
                let input = case.get("input");
                json!({
                    "expected": normalize_supported_protocols(input),
                    "known": supported_protocols_known(input),
                })
            })
            .collect::<Vec<_>>(),
        "reasoning_levels": fixture["reasoning_levels"]
            .as_array()
            .expect("reasoning cases")
            .iter()
            .map(|case| json!({"expected": normalize_reasoning_levels(case.get("input"))}))
            .collect::<Vec<_>>(),
    });
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn configured Python capability oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write Python capability fixture");
    let output = child
        .wait_with_output()
        .expect("wait for Python capability oracle");
    assert!(
        output.status.success(),
        "Python capability oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    assert_eq!(rust, oracle);
}
