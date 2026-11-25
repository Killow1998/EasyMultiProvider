use emp_config::{
    config::{LearnedProtocol, ProtocolSave, remember_learned_protocol},
    normalize_configuration,
    official_registry::bundled_snapshot,
};
use emp_domain::{Protocol, RouteSource, resolve_route_without_catalog, resolved_route_from_parts};
use serde_json::{Value, json};

const OBSERVED_AT: &str = "2026-09-22T06:00:00.123456+00:00";

fn auto_config() -> Value {
    normalize_configuration(Some(&json!({
        "providers":[{
            "id":"demo", "name":"Demo", "base_url":"https://example.invalid/v1",
            "protocol":"auto", "auth_mode":"api_key", "api_key":"test-key"
        }],
        "models":[{
            "id":"demo/model", "provider":"demo", "upstream_id":"upstream-model",
            "deployment_identity":"deployment-a"
        }]
    })))
    .expect("normalize auto config")
}

fn cases() -> Value {
    let auto = auto_config();
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
            let provider = config["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|provider| provider["id"] == case["provider_id"])
                .expect("provider in request config");
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
    assert_eq!(
        rust[2]["providers"][0]["protocol_observation"]["upstream_model"],
        "unlisted-model"
    );
    assert!(rust[3].is_null());
}

#[test]
fn removing_the_provider_invalidates_an_in_flight_candidate() {
    let mut config = auto_config();
    let route = resolve_route_without_catalog(&config, "demo/model").unwrap();
    let candidate =
        LearnedProtocol::from_route(&route, Protocol::Responses, OBSERVED_AT.to_owned()).unwrap();
    config["providers"] = json!([]);
    assert!(matches!(
        remember_learned_protocol(&config, &candidate, bundled_snapshot).unwrap(),
        ProtocolSave::IdentityChanged
    ));
}

#[test]
fn an_older_candidate_cannot_overwrite_a_newer_observation() {
    let config = auto_config();
    let route = resolve_route_without_catalog(&config, "demo/model").unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("data/python-provider-normalization.json")).unwrap();
    let legacy = fixture["valid"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["input"]["id"] == "native-forward")
        .unwrap()["input"]["protocol_observation"]["observed_at"]
        .as_str()
        .unwrap();
    for (earlier, later) in [
        ("2026-09-22T06:00:00+00:00", OBSERVED_AT),
        ("2026-08-20T23:59:59+00:00", legacy),
    ] {
        let earlier =
            LearnedProtocol::from_route(&route, Protocol::ChatCompletions, earlier.to_owned())
                .unwrap();
        let newer =
            LearnedProtocol::from_route(&route, Protocol::Responses, later.to_owned()).unwrap();
        let ProtocolSave::Updated(saved) =
            remember_learned_protocol(&config, &newer, bundled_snapshot).unwrap()
        else {
            panic!("the newer observation must save");
        };
        assert!(matches!(
            remember_learned_protocol(&saved, &earlier, bundled_snapshot).unwrap(),
            ProtocolSave::Superseded
        ));
        for entry in [&saved["providers"][0], &saved["models"][0]] {
            assert_eq!(entry["resolved_protocol"], "responses");
            assert_eq!(entry["protocol_observation"]["observed_at"], later);
        }
    }
}

#[test]
fn a_newer_provider_observation_does_not_block_an_older_alias_update() {
    let mut config = auto_config();
    let mut alias = config["models"][0].clone();
    alias["id"] = json!("demo/alias");
    config["models"].as_array_mut().unwrap().push(alias);
    let model = resolve_route_without_catalog(&config, "demo/model").unwrap();
    let alias = resolve_route_without_catalog(&config, "demo/alias").unwrap();

    let original =
        LearnedProtocol::from_route(&model, Protocol::ChatCompletions, OBSERVED_AT.to_owned())
            .unwrap();
    let model_time = "2026-09-22T06:00:01+00:00";
    let learn_model =
        LearnedProtocol::from_route(&model, Protocol::Responses, model_time.to_owned()).unwrap();
    let alias_time = "2026-09-22T06:00:02+00:00";
    let learn_alias =
        LearnedProtocol::from_route(&alias, Protocol::Responses, alias_time.to_owned()).unwrap();

    for candidate in [original, learn_alias, learn_model] {
        let ProtocolSave::Updated(updated) =
            remember_learned_protocol(&config, &candidate, bundled_snapshot).unwrap()
        else {
            panic!("at least one entry must update");
        };
        config = updated;
    }
    assert_eq!(
        config["providers"][0]["protocol_observation"]["observed_at"],
        alias_time
    );
    assert_eq!(config["models"][0]["resolved_protocol"], "responses");
    assert_eq!(
        config["models"][0]["protocol_observation"]["observed_at"],
        model_time
    );
    assert_eq!(
        config["models"][1]["protocol_observation"],
        config["providers"][0]["protocol_observation"]
    );
}
