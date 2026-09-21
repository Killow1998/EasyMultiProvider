use emp_state::normalize_configuration;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-config-normalization.json"
    ))
    .expect("valid full configuration fixture")
}

fn outcome(input: &Value) -> Value {
    match normalize_configuration(Some(input)) {
        Ok(value) => json!({"expected": value}),
        Err(error) => {
            json!({"error_type": error.python_type(), "error": error.to_string()})
        }
    }
}

fn outcomes(cases: &Value) -> Value {
    Value::Array(
        cases
            .as_array()
            .expect("configuration cases")
            .iter()
            .map(|case| outcome(&case["input"]))
            .collect(),
    )
}

#[test]
fn configuration_normalization_matches_frozen_python_fixture() {
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

#[test]
fn configuration_normalization_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.config import normalize
fixture = json.load(sys.stdin)
def outcome(case):
    try:
        return {"expected": normalize(case["input"])}
    except Exception as exc:
        return {"error_type": type(exc).__name__, "error": str(exc)}
json.dump({
    "valid": [outcome(case) for case in fixture["valid"]],
    "invalid": [outcome(case) for case in fixture["invalid"]],
    "default": normalize(None),
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
        .expect("spawn Python configuration oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write configuration fixture");
    let output = child
        .wait_with_output()
        .expect("wait for configuration oracle");
    assert!(
        output.status.success(),
        "Python configuration oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("configuration oracle JSON");
    assert_eq!(
        json!({
            "valid": outcomes(&fixture["valid"]),
            "invalid": outcomes(&fixture["invalid"]),
            "default": normalize_configuration(None).expect("default configuration"),
        }),
        oracle
    );
    assert_eq!(
        normalize_configuration(Some(&Value::Null)).expect("null configuration"),
        oracle["default"]
    );
}
