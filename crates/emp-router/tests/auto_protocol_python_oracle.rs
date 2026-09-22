use emp_core::{RouteSource, deployment_identity, endpoint_fingerprint, resolved_route_from_parts};
use emp_router::protocol_candidates;
use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn cases() -> Value {
    let observed_provider = json!({
        "id":"observed-provider", "base_url":"https://example.invalid/v1",
        "protocol":"auto", "auth_mode":"api_key", "api_key":"test-key"
    });
    let observed_model = json!({
        "id":"observed-provider/model", "provider":"observed-provider",
        "upstream_id":"upstream-model"
    });
    let provider_map = observed_provider.as_object().expect("provider object");
    let model_map = observed_model.as_object().expect("model object");
    let observation = json!({
        "endpoint_fingerprint": endpoint_fingerprint(Some("https://example.invalid/v1")),
        "deployment_identity": deployment_identity(provider_map, model_map),
        "upstream_model": "upstream-model"
    });
    json!([
        {
            "provider":{"id":"chat","base_url":"https://example.invalid/v1","protocol":"auto","auth_mode":"api_key","api_key":"test-key"},
            "model":{"id":"chat/model","provider":"chat","upstream_id":"upstream-model"}
        },
        {
            "provider":{"id":"responses","base_url":"https://example.invalid/v1/responses/","protocol":"auto","auth_mode":"api_key","api_key":"test-key"},
            "model":{"id":"responses/model","provider":"responses","upstream_id":"upstream-model"}
        },
        {
            "provider":{"id":"anthropic","base_url":"https://example.invalid/v1","protocol":"auto","auth_mode":"anthropic_api_key","api_key":"test-key"},
            "model":{"id":"anthropic/model","provider":"anthropic","upstream_id":"upstream-model"}
        },
        {
            "provider":observed_provider,
            "model":{
                "id":observed_model["id"], "provider":observed_model["provider"],
                "upstream_id":observed_model["upstream_id"],
                "resolved_protocol":"responses", "protocol_observation":observation
            }
        },
        {
            "provider":{
                "id":"stale", "base_url":"https://example.invalid/v1",
                "protocol":"auto", "auth_mode":"api_key", "api_key":"test-key",
                "resolved_protocol":"responses",
                "protocol_observation":{
                    "endpoint_fingerprint":format!("sha256:{}", "0".repeat(64)),
                    "deployment_identity":"default", "upstream_model":"upstream-model"
                }
            },
            "model":{"id":"stale/model","provider":"stale","upstream_id":"upstream-model"}
        }
    ])
}

fn rust_candidates(cases: &Value) -> Value {
    Value::Array(
        cases
            .as_array()
            .expect("cases array")
            .iter()
            .map(|case| {
                let provider = case["provider"]
                    .as_object()
                    .expect("provider object")
                    .clone();
                let model = case["model"].as_object().expect("model object").clone();
                let requested = model["id"].as_str().expect("model id").to_owned();
                let route = resolved_route_from_parts(
                    &requested,
                    provider,
                    model,
                    RouteSource::ExplicitModel,
                )
                .expect("auto route");
                Value::Array(
                    protocol_candidates(&route)
                        .into_iter()
                        .map(|protocol| Value::String(protocol.as_config_str().to_owned()))
                        .collect(),
                )
            })
            .collect(),
    )
}

fn python_candidates(cases: &Value) -> Option<Value> {
    let python = std::env::var("EMP_PYTHON_INTEROP").ok()?;
    let script = r#"
import json, sys
from easy_multi_provider.router import _auto_protocol_candidates
cases = json.load(sys.stdin)
json.dump([list(_auto_protocol_candidates(case["provider"], case["model"])) for case in cases], sys.stdout, separators=(",", ":"))
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
        .expect("spawn Python auto-protocol oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(cases)
                .expect("serialize cases")
                .as_bytes(),
        )
        .expect("write auto-protocol cases");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python auto-protocol oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(serde_json::from_slice(&output.stdout).expect("Python oracle JSON"))
}

#[test]
fn candidate_order_matches_live_python() {
    let cases = cases();
    let rust = rust_candidates(&cases);
    if let Some(python) = python_candidates(&cases) {
        assert_eq!(rust, python);
    }
    assert_eq!(
        rust,
        json!([
            ["chat_completions", "responses"],
            ["responses", "chat_completions"],
            ["anthropic_messages"],
            ["responses", "chat_completions"],
            ["chat_completions", "responses"]
        ])
    );
}
