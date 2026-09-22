use emp_state::{normalize_configuration, remember_resolved_protocol_at};
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const OBSERVED_AT: &str = "2026-09-22T06:00:00.123456+00:00";

fn cases() -> Value {
    let auto = normalize_configuration(Some(&json!({
        "providers":[{
            "id":"demo", "name":"Demo", "base_url":"https://example.invalid/v1",
            "protocol":"auto", "auth_mode":"api_key", "api_key":"test-key"
        }],
        "models":[{
            "id":"demo/model", "provider":"demo", "upstream_id":"upstream-model",
            "deployment_identity":"deployment-a"
        }]
    })))
    .expect("normalize auto config");
    let explicit = normalize_configuration(Some(&json!({
        "providers":[{
            "id":"explicit", "name":"Explicit", "base_url":"https://example.invalid/v1",
            "protocol":"chat_completions", "auth_mode":"api_key", "api_key":"test-key"
        }],
        "models":[{"id":"explicit/model","provider":"explicit","upstream_id":"upstream-model"}]
    })))
    .expect("normalize explicit config");
    json!([
        {"config":auto.clone(),"provider_id":"demo","model":"demo/model","protocol":"responses"},
        {"config":explicit,"provider_id":"explicit","model":"explicit/model","protocol":"responses"},
        {"config":auto.clone(),"provider_id":"missing","model":"demo/model","protocol":"responses"},
        {"config":auto.clone(),"provider_id":"demo","model":"unlisted-model","protocol":"chat_completions"},
        {"config":auto,"provider_id":"demo","model":"demo/model","protocol":"auto"}
    ])
}

fn rust_outcomes(cases: &Value) -> Value {
    Value::Array(
        cases
            .as_array()
            .expect("cases array")
            .iter()
            .map(|case| {
                remember_resolved_protocol_at(
                    &case["config"],
                    case["provider_id"].as_str().expect("provider id"),
                    case["model"].as_str().expect("model id"),
                    case["protocol"].as_str().expect("protocol"),
                    OBSERVED_AT,
                )
                .expect("remember protocol")
                .unwrap_or(Value::Null)
            })
            .collect(),
    )
}

fn python_outcomes(cases: &Value) -> Option<Value> {
    let python = std::env::var("EMP_PYTHON_INTEROP").ok()?;
    let script = r#"
import copy, json, sys
from easy_multi_provider.capabilities import deployment_identity, endpoint_fingerprint
from easy_multi_provider.config import normalize

observed_at = "2026-09-22T06:00:00.123456+00:00"
concrete = {"responses", "chat_completions", "anthropic_messages"}
result = []
for case in json.load(sys.stdin):
    if case["protocol"] not in concrete:
        result.append(None)
        continue
    config = copy.deepcopy(case["config"])
    provider = next((item for item in config.get("providers", [])
                     if item.get("id") == case["provider_id"] and item.get("protocol") == "auto"), None)
    if provider is None:
        result.append(None)
        continue
    model = next((item for item in config.get("models", [])
                  if item.get("id") == case["model"] and item.get("provider") == case["provider_id"]), None)
    observation = {
        "source":"observed", "confidence":1.0, "observed_at":observed_at,
        "endpoint_fingerprint":endpoint_fingerprint(provider.get("base_url")),
        "deployment_identity":deployment_identity(provider, model or {}),
        "upstream_model":(model or {}).get("upstream_id") or case["model"] or "",
    }
    provider["resolved_protocol"] = case["protocol"]
    provider["protocol_observation"] = observation
    if model is not None:
        model["resolved_protocol"] = case["protocol"]
        model["protocol_observation"] = dict(observation)
    result.append(normalize(config))
json.dump(result, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python protocol-observation oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(cases)
                .expect("serialize cases")
                .as_bytes(),
        )
        .expect("write observation cases");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python protocol-observation oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(serde_json::from_slice(&output.stdout).expect("Python oracle JSON"))
}

#[test]
fn successful_protocol_observations_match_live_python() {
    let cases = cases();
    let rust = rust_outcomes(&cases);
    if let Some(python) = python_outcomes(&cases) {
        assert_eq!(rust, python);
    }
    let first = &rust[0];
    assert_eq!(first["providers"][0]["resolved_protocol"], "responses");
    assert_eq!(first["models"][0]["resolved_protocol"], "responses");
    assert_eq!(
        first["providers"][0]["protocol_observation"],
        first["models"][0]["protocol_observation"]
    );
    assert!(rust[1].is_null());
    assert!(rust[2].is_null());
    assert_eq!(
        rust[3]["providers"][0]["protocol_observation"]["upstream_model"],
        "unlisted-model"
    );
    assert!(rust[4].is_null());
}
