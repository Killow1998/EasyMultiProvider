use super::*;
use emp_core::{RouteSource, resolved_route_from_parts};
use serde_json::json;

fn case(
    name: &str,
    protocol: &str,
    route_policy: Option<&str>,
    family: Option<&str>,
    family_policy: Option<&str>,
    supports_summary: bool,
) -> Value {
    let model_id = format!("fixture/{name}");
    let mut provider = json!({
        "id":"fixture", "name":"Fixture", "base_url":"https://fixture.invalid/v1",
        "protocol":protocol, "auth_mode":"api_key", "api_key":"fixture-key"
    });
    let mut model = json!({
        "id":model_id, "provider":"fixture", "upstream_id":"upstream-model",
        "supports_reasoning_summaries":supports_summary,
        "future_model_field":{"opaque":true}
    });
    if let Some(family) = family {
        model["family_id"] = json!(family);
    } else {
        model.as_object_mut().unwrap().remove("upstream_id");
    }
    if name == "native-unchanged" {
        provider["id"] = json!("native");
        provider["auth_mode"] = json!("forward");
        model["provider"] = json!("native");
        model["id"] = json!(format!("native/{name}"));
    }
    if name == "auto-url-candidate-only" {
        provider["base_url"] = json!("https://fixture.invalid/v1/responses");
    }
    let mut route_presentations = serde_json::Map::new();
    if let Some(policy) = route_policy {
        route_presentations.insert(
            model["id"].as_str().unwrap().to_owned(),
            json!({"reasoning_summary":policy}),
        );
    }
    let mut family_presentations = serde_json::Map::new();
    if let Some(family) = family {
        if let Some(policy) = family_policy {
            family_presentations.insert(family.to_owned(), json!({"reasoning_summary":policy}));
        } else if name == "family-empty-overrides-route" {
            family_presentations.insert(family.to_owned(), json!({}));
        }
    } else if let Some(policy) = family_policy {
        family_presentations.insert(
            model["id"].as_str().unwrap().to_owned(),
            json!({
                "reasoning_summary":policy
            }),
        );
    }
    json!({
        "name":name,
        "route":model["id"],
        "config":{
            "providers":[provider.clone()], "models":[model.clone()],
            "catalog_presentations":route_presentations,
            "catalog_family_presentations":family_presentations
        },
        "provider":provider,
        "model":model,
        "body":{
            "model":model["id"], "input":"fixture input",
            "reasoning":{"summary":"detailed","effort":"low","future_reasoning":{"opaque":true}},
            "future_body_field":{"opaque":true}
        }
    })
}

fn cases() -> Vec<Value> {
    let mut cases = vec![
        case(
            "responses-auto",
            "responses",
            None,
            Some("shared"),
            None,
            true,
        ),
        case(
            "responses-show",
            "responses",
            Some("show"),
            Some("show-family"),
            None,
            true,
        ),
        case(
            "responses-hide",
            "responses",
            Some("hide"),
            Some("hide-family"),
            None,
            true,
        ),
        case(
            "show-unsupported",
            "responses",
            Some("show"),
            Some("unsupported-family"),
            None,
            false,
        ),
        case(
            "chat-nonresponses",
            "chat_completions",
            Some("show"),
            Some("chat-family"),
            None,
            true,
        ),
        case(
            "auto-url-candidate-only",
            "auto",
            None,
            Some("auto-family"),
            None,
            true,
        ),
        case(
            "route-policy-without-family",
            "responses",
            Some("hide"),
            None,
            Some("show"),
            true,
        ),
        case(
            "family-show-precedence",
            "responses",
            Some("hide"),
            Some("shared-family"),
            Some("show"),
            true,
        ),
        case(
            "family-empty-overrides-route",
            "responses",
            Some("hide"),
            Some("empty-family"),
            None,
            true,
        ),
        case(
            "native-unchanged",
            "responses",
            Some("hide"),
            Some("native-family"),
            Some("show"),
            true,
        ),
    ];
    for name in ["auto-matched-observation", "auto-stale-observation"] {
        let mut fixture = case(name, "auto", None, Some(name), None, true);
        fixture["provider"]["base_url"] = json!("https://fixture.invalid/v1");
        fixture["config"]["providers"][0]["base_url"] = fixture["provider"]["base_url"].clone();
        let provider = fixture["provider"].as_object().unwrap().clone();
        let model = fixture["model"].as_object().unwrap().clone();
        let provisional = resolved_route_from_parts(
            fixture["route"].as_str().unwrap(),
            provider,
            model,
            RouteSource::ExplicitModel,
        )
        .unwrap();
        let fingerprint = if name == "auto-stale-observation" {
            format!("sha256:{}", "0".repeat(64))
        } else {
            provisional.endpoint_fingerprint
        };
        fixture["model"]["resolved_protocol"] = json!("responses");
        fixture["model"]["protocol_observation"] = json!({
            "endpoint_fingerprint":fingerprint,
            "deployment_identity":provisional.deployment_identity,
            "upstream_model":"upstream-model"
        });
        fixture["config"]["models"][0] = fixture["model"].clone();
        cases.push(fixture);
    }
    cases
}

#[test]
fn external_request_preparation_applies_summary_policy_once_and_keeps_unknown_fields() {
    for fixture in cases() {
        let route = resolved_route_from_parts(
            fixture["route"].as_str().unwrap(),
            fixture["provider"].as_object().unwrap().clone(),
            fixture["model"].as_object().unwrap().clone(),
            RouteSource::ExplicitModel,
        )
        .unwrap();
        let (route, body) =
            prepare_external_request(&fixture["config"], route, fixture["body"].clone())
                .expect("prepare request snapshot");
        let model = route.model.value();
        let name = fixture["name"].as_str().unwrap();
        let expected_preserve = matches!(
            name,
            "responses-auto"
                | "responses-show"
                | "family-show-precedence"
                | "family-empty-overrides-route"
                | "auto-matched-observation"
        );
        let expected_policy = match name {
            "responses-show"
            | "show-unsupported"
            | "chat-nonresponses"
            | "family-show-precedence" => "show",
            "responses-hide" | "route-policy-without-family" => "hide",
            "native-unchanged" => "",
            _ => "auto",
        };
        match name {
            "responses-auto" => assert_eq!(body["reasoning"]["summary"], "detailed"),
            "responses-show" | "family-show-precedence" => {
                assert_eq!(body["reasoning"]["summary"], "auto")
            }
            "auto-matched-observation" => {
                assert_eq!(body["reasoning"]["summary"], "detailed")
            }
            "family-empty-overrides-route" => {
                assert_eq!(body["reasoning"]["summary"], "detailed")
            }
            "native-unchanged" => {
                assert_eq!(body, fixture["body"]);
                assert!(model.get("_emp_reasoning_summary_policy").is_none());
                assert!(model.get("_emp_preserve_reasoning_summary").is_none());
                assert!(model.get("_emp_preserve_reasoning_state").is_none());
            }
            _ => assert!(body["reasoning"].get("summary").is_none(), "{name}"),
        }
        assert_eq!(body["future_body_field"], json!({"opaque":true}));
        assert_eq!(
            model.get("future_model_field"),
            Some(&json!({"opaque":true}))
        );
        if name != "native-unchanged" {
            assert_eq!(
                model.get("_emp_reasoning_summary_policy"),
                Some(&json!(expected_policy)),
                "{name}"
            );
            assert_eq!(
                model.get("_emp_preserve_reasoning_summary"),
                Some(&json!(expected_preserve)),
                "{name}"
            );
            assert_eq!(
                model.get("_emp_preserve_reasoning_state"),
                Some(&json!(false)),
                "{name}"
            );
        }
    }
}
