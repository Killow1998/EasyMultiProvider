use emp_state::normalize_model;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-model-normalization.json"
    ))
    .expect("valid model normalization fixture")
}

fn rust_outcomes(cases: &Value) -> Value {
    Value::Array(
        cases
            .as_array()
            .expect("model cases")
            .iter()
            .map(|case| match normalize_model(&case["input"]) {
                Ok(value) => json!({"expected": value}),
                Err(error) => {
                    json!({"error_type": error.python_type(), "error": error.to_string()})
                }
            })
            .collect(),
    )
}

#[test]
fn model_normalization_matches_frozen_python_fixture() {
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

#[test]
fn model_normalization_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.config import _normalize_model
fixture = json.load(sys.stdin)
def outcome(case):
    try:
        return {"expected": _normalize_model(case["input"])}
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
        .expect("spawn Python model oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write model fixture");
    let output = child.wait_with_output().expect("wait for model oracle");
    assert!(
        output.status.success(),
        "Python model oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("model oracle JSON");
    assert_eq!(
        json!({
            "valid": rust_outcomes(&fixture["valid"]),
            "invalid": rust_outcomes(&fixture["invalid"]),
        }),
        oracle
    );
}
