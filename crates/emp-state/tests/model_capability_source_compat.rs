use emp_state::normalize_model_capability_sources;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

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

fn rust_outcomes(fixture: &Value, section: &Value) -> Value {
    Value::Array(
        section
            .as_array()
            .expect("capability-source cases")
            .iter()
            .map(|case| {
                let explicit = explicit_fields(fixture, case);
                match normalize_model_capability_sources(
                    case.get("input"),
                    case_values(fixture, case),
                    Some(&explicit),
                ) {
                    Ok(value) => json!({"expected": value}),
                    Err(error) => json!({"error": error.to_string()}),
                }
            })
            .collect(),
    )
}

#[test]
fn model_capability_sources_match_frozen_python_fixture() {
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

#[test]
fn model_capability_sources_match_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.config import _normalize_capability_sources
fixture = json.load(sys.stdin)
def outcome(case):
    try:
        values = case.get("values", fixture["values"])
        explicit = set(case.get("explicit_fields", fixture["explicit_fields"]))
        return {"expected": _normalize_capability_sources(case["input"], values, explicit)}
    except Exception as exc:
        return {"error": str(exc)}
json.dump({
    "valid": [outcome(case) for case in fixture["valid"]],
    "invalid": [outcome(case) for case in fixture["invalid"]],
}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python capability-source oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write capability-source fixture");
    let output = child
        .wait_with_output()
        .expect("wait for capability oracle");
    assert!(
        output.status.success(),
        "Python capability-source oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value =
        serde_json::from_slice(&output.stdout).expect("capability-source oracle JSON");
    assert_eq!(
        json!({
            "valid": rust_outcomes(&fixture, &fixture["valid"]),
            "invalid": rust_outcomes(&fixture, &fixture["invalid"]),
        }),
        oracle
    );
}
