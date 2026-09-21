use emp_state::normalize_provider;
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-provider-normalization.json"
    ))
    .expect("valid provider fixture")
}

fn rust_outcomes(cases: &Value) -> Value {
    Value::Array(
        cases
            .as_array()
            .expect("provider cases")
            .iter()
            .map(|case| match normalize_provider(&case["input"]) {
                Ok(value) => json!({"expected": value}),
                Err(error) => json!({"error": error.to_string()}),
            })
            .collect(),
    )
}

#[test]
fn provider_normalization_matches_frozen_python_fixture() {
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

#[test]
fn provider_normalization_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.config import _normalize_provider
fixture = json.load(sys.stdin)
def outcome(case):
    try:
        return {"expected": _normalize_provider(case["input"])}
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
        .expect("spawn Python provider oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write provider fixture");
    let output = child.wait_with_output().expect("wait for provider oracle");
    assert!(
        output.status.success(),
        "Python provider oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("provider oracle JSON");
    assert_eq!(
        json!({
            "valid": rust_outcomes(&fixture["valid"]),
            "invalid": rust_outcomes(&fixture["invalid"]),
        }),
        oracle
    );
}
