use emp_state::{normalize_configuration, normalize_provider};
use serde_json::{Value, json};

/// Frozen provider fixture. Failure cases assert the stable key fragment
/// (`provider.<field>`) instead of the full sentence.
fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/state/python-provider-normalization.json"
    ))
    .expect("valid provider fixture")
}

fn error_fragment(input: &Value) -> String {
    if !input.is_object() {
        return "provider must be an object".to_owned();
    }
    if input.get("id").is_none() {
        return "provider.id".to_owned();
    }
    if input.get("base_url").is_none() {
        return "provider.base_url".to_owned();
    }
    if let Some(field) = [
        "name",
        "api_key",
        "api_key_file",
        "deployment_identity",
        "protocol_observation",
        "capabilities",
        "resolved_protocol",
    ]
    .into_iter()
    .find(|field| input.get(*field).is_some())
    {
        return match field {
            "name" | "api_key" | "api_key_file" => "value must be a string".to_owned(),
            "resolved_protocol" => "resolved_protocol".to_owned(),
            "protocol_observation" => "protocol_observation".to_owned(),
            field => format!("provider.{field}"),
        };
    }
    if let Some(auth_mode) = input.get("auth_mode") {
        // auth_mode errors come first; forward + non-Responses protocol is
        // rejected as a combination.
        let protocol = input.get("protocol").and_then(Value::as_str);
        if auth_mode == "forward" && protocol != Some("responses") {
            return "forward providers must use the Responses protocol".to_owned();
        }
        return "provider.auth_mode".to_owned();
    }
    if input.get("protocol").is_some() {
        return "provider.protocol".to_owned();
    }
    panic!("unmapped provider error input {input}")
}

#[test]
fn provider_normalization_matches_frozen_fixture() {
    let fixture = fixture();
    for case in fixture["valid"].as_array().expect("valid providers") {
        assert_eq!(
            normalize_provider(&case["input"]).expect("valid provider"),
            case["expected"]
        );
    }
    for case in fixture["invalid"].as_array().expect("invalid providers") {
        let error = normalize_provider(&case["input"]).expect_err("invalid provider");
        let fragment = error_fragment(&case["input"]);
        assert!(
            error.to_string().contains(&fragment),
            "input {}: {error:?} must name {fragment:?}",
            case["input"]
        );
    }
}

#[test]
fn provider_normalization_rejects_unusable_values_with_field_fragments() {
    // Wrong-type and unsupported-enum failures name the provider field.
    let base = json!({"id": "demo", "base_url": "https://example.test/v1"});
    for (mut input, fragment) in [
        (json!({ "protocol": "rpc"}), "provider.protocol"),
        (json!({ "auth_mode": "delegate"}), "provider.auth_mode"),
        (json!({ "api_key": []}), "value must be a string"),
        (
            json!({ "protocol_observation": {"confidence": 1.1}}),
            "invalid protocol_observation",
        ),
        (
            json!({ "capabilities": {"streaming": "yes"}}),
            "provider.capabilities.streaming",
        ),
    ] {
        // Valid base_url first so validation reaches the target field.
        let mut merged = base.as_object().expect("base object").clone();
        for (key, value) in input.as_object().expect("case object") {
            merged.insert(key.clone(), value.clone());
        }
        input = Value::Object(merged);
        let error = normalize_provider(&input).expect_err("invalid provider");
        assert!(
            error.to_string().contains(fragment),
            "{input}: {error:?} must name {fragment:?}"
        );
    }
}

#[test]
fn local_cli_login_round_trips_and_rejects_mixed_credentials() {
    for (backend, auth, protocol) in [
        ("claude_cli", "claude_login", "anthropic_messages"),
        ("agy_cli", "agy_login", "responses"),
    ] {
        let raw = json!({"providers":[{"id":"local", "protocol":protocol,"execution_backend":backend,"auth_mode":auth}],"models":[{"id":"local/model","provider":"local","upstream_id":"model"}]});
        let normalized = normalize_configuration(Some(&raw)).unwrap();
        let round_trip = normalize_configuration(Some(&normalized)).unwrap();
        assert_eq!(round_trip, normalized);
        let provider = &round_trip["providers"][0];
        for key in ["base_url", "api_key", "api_key_file"] {
            assert_eq!(provider[key], "");
            let mut invalid = provider.clone();
            invalid[key] = json!(if key == "base_url" {
                "https://example.invalid"
            } else {
                "synthetic"
            });
            assert!(normalize_provider(&invalid).is_err());
        }
        let mut invalid = provider.clone();
        invalid["execution_backend"] = json!("http");
        assert!(normalize_provider(&invalid).is_err());
    }
}
