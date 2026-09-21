use emp_state::normalize_context_calibrations;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-context-calibrations.json"
    ))
    .expect("valid context calibration fixture")
}

fn outcome(input: &Value) -> Value {
    match normalize_context_calibrations(Some(input)) {
        Ok(value) => json!({"expected": value}),
        Err(error) => json!({"error_type": "ConfigError", "error": error.to_string()}),
    }
}

#[test]
fn context_calibrations_match_frozen_python_fixture() {
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
fn null_context_calibrations_match_python() {
    assert_eq!(
        normalize_context_calibrations(None).expect("null normalization"),
        Value::Array(Vec::new()),
    );
}

#[test]
fn context_calibrations_match_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.config import _normalize_context_calibrations
fixture = json.load(sys.stdin)
def outcome(case):
    try:
        return {"expected": _normalize_context_calibrations(case["input"])}
    except Exception as exc:
        return {"error_type": type(exc).__name__, "error": str(exc)}
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
        .expect("spawn Python context calibration oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write context calibration fixture");
    let output = child
        .wait_with_output()
        .expect("wait for context calibration oracle");
    assert!(
        output.status.success(),
        "Python context calibration oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value =
        serde_json::from_slice(&output.stdout).expect("context calibration oracle JSON");
    assert_eq!(
        json!({
            "valid": fixture["valid"]
                .as_array()
                .expect("valid cases")
                .iter()
                .map(|case| outcome(&case["input"]))
                .collect::<Vec<_>>(),
            "invalid": fixture["invalid"]
                .as_array()
                .expect("invalid cases")
                .iter()
                .map(|case| outcome(&case["input"]))
                .collect::<Vec<_>>(),
        }),
        oracle,
    );
}
