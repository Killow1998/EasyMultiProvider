use emp_state::{merge_web_update_with_time, normalize_configuration};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-web-update-merge.json"
    ))
    .expect("valid Web-update merge fixture")
}

fn outcome(case: &Value, observed_at: &str) -> Value {
    let current = match normalize_configuration(Some(&case["current"])) {
        Ok(current) => current,
        Err(error) => {
            return json!({"error_type": error.python_type(), "error": error.to_string()});
        }
    };
    match merge_web_update_with_time(&current, &case["incoming"], observed_at) {
        Ok(value) => json!({"expected": value}),
        Err(error) => json!({"error_type": error.python_type(), "error": error.to_string()}),
    }
}

#[test]
fn web_update_merge_matches_frozen_python_fixture() {
    let fixture = fixture();
    let observed_at = fixture["fixed_observed_at"]
        .as_str()
        .expect("fixed observed_at");
    for case in fixture["valid"].as_array().expect("valid merge cases") {
        let current =
            normalize_configuration(Some(&case["current"])).expect("valid current configuration");
        let actual = merge_web_update_with_time(&current, &case["incoming"], observed_at)
            .expect("valid Web update");
        assert_eq!(actual.as_object().map(|object| object.len()), Some(15));
        for pointer in case["select"].as_array().expect("selected paths") {
            let pointer = pointer.as_str().expect("JSON pointer");
            assert_eq!(
                actual.pointer(pointer),
                case["expected"].get(pointer),
                "case: {}, pointer: {pointer}",
                case["name"]
            );
        }
    }
    for case in fixture["invalid"].as_array().expect("invalid merge cases") {
        let current =
            normalize_configuration(Some(&case["current"])).expect("valid current configuration");
        let error = merge_web_update_with_time(&current, &case["incoming"], observed_at)
            .expect_err("invalid Web update");
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
fn web_update_merge_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let observed_at = fixture["fixed_observed_at"]
        .as_str()
        .expect("fixed observed_at");
    let script = r#"
import json, sys
from easy_multi_provider import config as config_module
fixture = json.load(sys.stdin)
config_module.observed_at_now = lambda: fixture["fixed_observed_at"]
def outcome(case):
    try:
        current = config_module.normalize(case["current"])
        return {"expected": config_module.merge_web_update(current, case["incoming"])}
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
        .expect("spawn Python Web-update oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write Web-update fixture");
    let output = child
        .wait_with_output()
        .expect("wait for Web-update oracle");
    assert!(
        output.status.success(),
        "Python Web-update oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Web-update oracle JSON");
    let actual = json!({
        "valid": fixture["valid"]
            .as_array()
            .expect("valid merge cases")
            .iter()
            .map(|case| outcome(case, observed_at))
            .collect::<Vec<_>>(),
        "invalid": fixture["invalid"]
            .as_array()
            .expect("invalid merge cases")
            .iter()
            .map(|case| outcome(case, observed_at))
            .collect::<Vec<_>>(),
    });
    assert_eq!(actual, oracle);
}
