use emp_state::{normalize_configuration, remember_resolved_protocol_at};
use serde_json::{Value, json};

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

#[test]
fn successful_protocol_observations_update_provider_and_model_state() {
    let cases = cases();
    let rust = rust_outcomes(&cases);
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
