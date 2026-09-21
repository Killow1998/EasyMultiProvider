use emp_state::{normalize_account, normalize_context_windows, normalize_hidden_models};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-account-normalization.json"
    ))
    .expect("valid synthetic Python account fixture")
}

fn outcome(result: Result<Value, emp_state::AccountError>) -> Value {
    match result {
        Ok(value) => json!({"expected": value}),
        Err(error) => json!({"error": error.to_string()}),
    }
}

#[test]
fn account_normalization_matches_frozen_python_fixture() {
    let fixture = fixture();
    for case in fixture["hidden_models"]["valid"]
        .as_array()
        .expect("valid hidden model cases")
    {
        assert_eq!(
            normalize_hidden_models(case.get("input"), "account.hidden_models")
                .map(Value::from)
                .expect("valid hidden models"),
            case["expected"]
        );
    }
    for case in fixture["hidden_models"]["invalid"]
        .as_array()
        .expect("invalid hidden model cases")
    {
        assert_eq!(
            normalize_hidden_models(Some(&case["input"]), "account.hidden_models")
                .expect_err("invalid hidden models")
                .to_string(),
            case["error"].as_str().expect("hidden model error")
        );
    }
    for case in fixture["context_windows"]["valid"]
        .as_array()
        .expect("valid context cases")
    {
        assert_eq!(
            Value::Object(
                normalize_context_windows(case.get("input")).expect("valid context windows")
            ),
            case["expected"]
        );
    }
    for case in fixture["context_windows"]["invalid"]
        .as_array()
        .expect("invalid context cases")
    {
        assert_eq!(
            normalize_context_windows(Some(&case["input"]))
                .expect_err("invalid context windows")
                .to_string(),
            case["error"].as_str().expect("context error")
        );
    }
    for case in fixture["accounts"]["valid"]
        .as_array()
        .expect("valid account cases")
    {
        assert_eq!(
            normalize_account(&case["input"]).expect("valid account"),
            case["expected"]
        );
    }
    for case in fixture["accounts"]["invalid"]
        .as_array()
        .expect("invalid account cases")
    {
        assert_eq!(
            normalize_account(&case["input"])
                .expect_err("invalid account")
                .to_string(),
            case["error"].as_str().expect("account error")
        );
    }
}

#[test]
fn account_bounds_use_utf8_bytes_and_reject_boolean_integers() {
    let fixture = fixture();
    let bounds = &fixture["bounds"];
    let model_limit = bounds["hidden_models_limit"].as_u64().expect("model limit") as usize;
    let model_byte_limit = bounds["model_id_limit_bytes"]
        .as_u64()
        .expect("model byte limit") as usize;
    let segment_limit = bounds["segment_limit_bytes"]
        .as_u64()
        .expect("segment limit") as usize;

    assert_eq!(
        normalize_hidden_models(
            Some(&json!(vec!["x"; model_limit + 1])),
            "account.hidden_models"
        )
        .expect_err("too many hidden models")
        .to_string(),
        bounds["too_many_hidden_models_error"]
            .as_str()
            .expect("error")
    );
    let mut windows = serde_json::Map::new();
    for index in 0..=model_limit {
        windows.insert(format!("model-{index}"), json!(1));
    }
    assert_eq!(
        normalize_context_windows(Some(&Value::Object(windows)))
            .expect_err("too many context windows")
            .to_string(),
        bounds["too_many_context_windows_error"]
            .as_str()
            .expect("error")
    );

    let long_bytes = "x".repeat(model_byte_limit + 1);
    assert_eq!(
        normalize_hidden_models(Some(&json!([long_bytes])), "account.hidden_models")
            .expect_err("long model bytes")
            .to_string(),
        "account.hidden_models contains an oversized model ID"
    );
    let long_segment = "x".repeat(segment_limit + 1);
    assert_eq!(
        normalize_account(&json!({"id": long_segment, "prefix": "p"}))
            .expect_err("long account ID")
            .to_string(),
        bounds["too_long_segment_error"].as_str().expect("error")
    );

    let padded_model = format!("{}x", " ".repeat(model_byte_limit));
    assert_eq!(padded_model.len(), model_byte_limit + 1);
    assert_eq!(
        normalize_context_windows(Some(&json!({padded_model: 1})))
            .expect_err("original context-window key is oversized")
            .to_string(),
        "model_context_windows has an invalid model ID"
    );
}

#[test]
fn quota_is_cloned_without_retaining_input_references() {
    let mut input = json!({
        "id": "quota-account",
        "prefix": "quota-account",
        "quota": {"remaining": 3}
    });
    let normalized = normalize_account(&input).expect("valid account");
    input["quota"]["remaining"] = json!(99);
    assert_eq!(normalized["quota"], json!({"remaining": 3}));
    assert!(
        normalize_account(&json!({"id": "none", "prefix": "none", "quota": []}))
            .expect("valid account")["quota"]
            .is_null()
    );
}

#[test]
fn account_normalization_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import json, sys
from easy_multi_provider.accounts import (
    normalize_account,
    normalize_context_windows,
    normalize_hidden_models,
)
fixture = json.load(sys.stdin)
def outcome(function, case):
    try:
        return {"expected": function(case["input"], case["field"]) if "field" in case else function(case["input"])}
    except Exception as exc:
        return {"error": str(exc)}
def account_outcome(case):
    try:
        return {"expected": normalize_account(case["input"])}
    except Exception as exc:
        return {"error": str(exc)}
def hidden_outcome(case):
    return outcome(lambda value, field="account.hidden_models": normalize_hidden_models(value, field), case)
json.dump({
    "hidden_models": [hidden_outcome(case) for case in fixture["hidden_models"]["valid"] + fixture["hidden_models"]["invalid"]],
    "context_windows": [outcome(normalize_context_windows, case) for case in fixture["context_windows"]["valid"] + fixture["context_windows"]["invalid"]],
    "accounts": [account_outcome(case) for case in fixture["accounts"]["valid"] + fixture["accounts"]["invalid"]],
}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let rust = json!({
        "hidden_models": fixture["hidden_models"]["valid"]
            .as_array()
            .expect("valid hidden cases")
            .iter()
            .chain(fixture["hidden_models"]["invalid"].as_array().expect("invalid hidden cases"))
            .map(|case| {
                let result = normalize_hidden_models(case.get("input"), "account.hidden_models")
                    .map(Value::from);
                outcome(result)
            })
            .collect::<Vec<_>>(),
        "context_windows": fixture["context_windows"]["valid"]
            .as_array()
            .expect("valid context cases")
            .iter()
            .chain(fixture["context_windows"]["invalid"].as_array().expect("invalid context cases"))
            .map(|case| {
                outcome(
                    normalize_context_windows(case.get("input")).map(Value::Object),
                )
            })
            .collect::<Vec<_>>(),
        "accounts": fixture["accounts"]["valid"]
            .as_array()
            .expect("valid accounts")
            .iter()
            .chain(fixture["accounts"]["invalid"].as_array().expect("invalid accounts"))
            .map(|case| outcome(normalize_account(&case["input"])))
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
        .expect("spawn configured Python account oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write Python account fixture");
    let output = child
        .wait_with_output()
        .expect("wait for Python account oracle");
    assert!(
        output.status.success(),
        "Python account oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python account oracle JSON");
    assert_eq!(
        json!({"hidden_models": rust["hidden_models"], "context_windows": rust["context_windows"], "accounts": rust["accounts"]}),
        oracle
    );
}
