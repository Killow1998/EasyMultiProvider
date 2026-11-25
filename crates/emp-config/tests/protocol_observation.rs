use emp_config::{
    config::{LearnedProtocol, ProtocolSave, remember_learned_protocol},
    normalize_configuration,
    official_registry::bundled_snapshot,
};
use emp_domain::{Protocol, RouteSource, resolve_route_without_catalog, resolved_route_from_parts};
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

fn outcomes(cases: &Value) -> Vec<Value> {
    cases
        .as_array()
        .expect("cases array")
        .iter()
        .map(|case| {
            let config = &case["config"];
            let model = case["model"].as_str().expect("model id");
            let Some(provider) = config["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|provider| provider["id"] == case["provider_id"])
            else {
                return Value::Null;
            };
            let route = resolve_route_without_catalog(config, model).unwrap_or_else(|_| {
                resolved_route_from_parts(
                    model,
                    provider.as_object().unwrap().clone(),
                    json!({"id":model}).as_object().unwrap().clone(),
                    RouteSource::ForwardProvider,
                )
                .unwrap()
            });
            let protocol: Protocol = serde_json::from_value(case["protocol"].clone()).unwrap();
            let Some(candidate) =
                LearnedProtocol::from_route(&route, protocol, OBSERVED_AT.to_owned())
            else {
                return Value::Null;
            };
            match remember_learned_protocol(config, &candidate, bundled_snapshot).unwrap() {
                ProtocolSave::Updated(updated) => updated,
                _ => Value::Null,
            }
        })
        .collect()
}

#[test]
fn successful_protocol_observations_update_provider_and_model_state() {
    let cases = cases();
    let rust = outcomes(&cases);
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

#[test]
fn an_older_candidate_cannot_overwrite_a_newer_observation() {
    let cases = cases();
    let config = &cases[0]["config"];
    let route = resolve_route_without_catalog(config, "demo/model").unwrap();
    let earlier = LearnedProtocol::from_route(
        &route,
        Protocol::ChatCompletions,
        "2026-09-22T06:00:00+00:00".to_owned(),
    )
    .unwrap();
    let later =
        LearnedProtocol::from_route(&route, Protocol::Responses, OBSERVED_AT.to_owned()).unwrap();
    let ProtocolSave::Updated(saved) =
        remember_learned_protocol(config, &later, bundled_snapshot).unwrap()
    else {
        panic!("the newer observation must save");
    };
    assert!(matches!(
        remember_learned_protocol(&saved, &earlier, bundled_snapshot).unwrap(),
        ProtocolSave::Superseded
    ));
    for entry in [&saved["providers"][0], &saved["models"][0]] {
        assert_eq!(entry["resolved_protocol"], "responses");
        assert_eq!(entry["protocol_observation"]["observed_at"], OBSERVED_AT);
    }
}
