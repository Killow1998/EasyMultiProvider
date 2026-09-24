//! One-time, request-local preparation shared by portable Responses endpoints.
//!
//! Python's `_prepare_model_request` resolves the summary policy from the full
//! config only after the destination route is frozen. Keep the request body and
//! route snapshot paired so projection, context checks, and forwarding observe
//! the same capability decision without persisting internal markers.

use emp_core::{Dialect, OpaqueJson, Protocol, ResolvedRoute};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryPolicy {
    Auto,
    Show,
    Hide,
}

impl SummaryPolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Show => "show",
            Self::Hide => "hide",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestPreparationError {
    ModelSnapshotTooLarge,
}

/// Apply Python's external reasoning-summary policy exactly once to an owned
/// body and its frozen route. Native routes keep both inputs untouched.
pub(crate) fn prepare_external_request(
    config: &Value,
    mut route: ResolvedRoute,
    mut body: Value,
) -> Result<(ResolvedRoute, Value), RequestPreparationError> {
    if route.dialect == Dialect::CodexNative {
        return Ok((route, body));
    }

    let model = route.model.value();
    let policy = summary_policy(config, &route, model);
    let supported = model
        .get("supports_reasoning_summaries")
        .and_then(Value::as_bool)
        == Some(true);
    let responses_capable = route.protocol == Protocol::Responses
        || (route.protocol == Protocol::Auto
            && emp_router::observed_protocol(&route) == Some(Protocol::Responses));
    let preserve_summary = supported && responses_capable && policy != SummaryPolicy::Hide;

    let mut request_model = model.clone();
    request_model.insert(
        "_emp_reasoning_summary_policy".to_owned(),
        Value::String(policy.as_str().to_owned()),
    );
    request_model.insert(
        "_emp_preserve_reasoning_summary".to_owned(),
        Value::Bool(preserve_summary),
    );
    request_model.insert(
        "_emp_preserve_reasoning_state".to_owned(),
        Value::Bool(false),
    );
    route.model = OpaqueJson::new(request_model)
        .map_err(|_| RequestPreparationError::ModelSnapshotTooLarge)?;

    if let Some(body) = body.as_object_mut() {
        let mut reasoning = body
            .get("reasoning")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !responses_capable || !supported || policy == SummaryPolicy::Hide {
            reasoning.remove("summary");
        } else if policy == SummaryPolicy::Show {
            reasoning.insert("summary".to_owned(), Value::String("auto".to_owned()));
        }
        if reasoning.is_empty() {
            body.remove("reasoning");
        } else {
            body.insert("reasoning".to_owned(), Value::Object(reasoning));
        }
    }
    Ok((route, body))
}

fn summary_policy(
    config: &Value,
    route: &ResolvedRoute,
    model: &Map<String, Value>,
) -> SummaryPolicy {
    let explicit_family = ["family_id", "upstream_id"].iter().any(|field| {
        model
            .get(*field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let family = model
        .get("_emp_family")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| nonempty_model_field(model, "family_id"))
        .or_else(|| nonempty_model_field(model, "upstream_id"))
        .unwrap_or(&route.requested_model);

    let family_presentation = explicit_family
        .then(|| {
            config
                .get("catalog_family_presentations")
                .and_then(Value::as_object)
                .and_then(|presentations| presentations.get(family))
                .and_then(Value::as_object)
        })
        .flatten();
    let presentation = family_presentation.or_else(|| {
        config
            .get("catalog_presentations")
            .and_then(Value::as_object)
            .and_then(|presentations| presentations.get(&route.requested_model))
            .and_then(Value::as_object)
    });
    match presentation
        .and_then(|value| value.get("reasoning_summary"))
        .and_then(Value::as_str)
    {
        Some("show") => SummaryPolicy::Show,
        Some("hide") => SummaryPolicy::Hide,
        _ => SummaryPolicy::Auto,
    }
}

fn nonempty_model_field<'a>(model: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    model
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use emp_core::{RouteSource, resolved_route_from_parts};
    use serde_json::json;
    use std::io::Write;
    use std::process::{Command, Stdio};

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

    fn route_for(case: &Value) -> ResolvedRoute {
        resolved_route_from_parts(
            case["route"].as_str().unwrap(),
            case["provider"].as_object().unwrap().clone(),
            case["model"].as_object().unwrap().clone(),
            RouteSource::ExplicitModel,
        )
        .unwrap()
    }

    fn rust_results(cases: &[Value]) -> Value {
        Value::Array(
            cases
                .iter()
                .map(|case| {
                    let (route, body) = prepare_external_request(
                        &case["config"],
                        route_for(case),
                        case["body"].clone(),
                    )
                    .expect("prepare request snapshot");
                    let model = route.model.value();
                    json!({
                        "body":body,
                        "policy":model.get("_emp_reasoning_summary_policy").cloned().unwrap_or(Value::Null),
                        "preserve":model.get("_emp_preserve_reasoning_summary").cloned().unwrap_or(Value::Null),
                        "state":model.get("_emp_preserve_reasoning_state").cloned().unwrap_or(Value::Null),
                        "future_model_field":model.get("future_model_field").cloned().unwrap_or(Value::Null)
                    })
                })
                .collect(),
        )
    }

    fn python_results(cases: &[Value]) -> Option<Value> {
        let python = std::env::var("EMP_PYTHON_INTEROP").ok()?;
        let root = std::env::var("EMP_PYTHON_ORACLE_ROOT").ok()?;
        let script = r#"
import json, sys
import easy_multi_provider
from easy_multi_provider.router import _prepare_model_request
assert easy_multi_provider.__version__ == "0.11.10"
cases = json.load(sys.stdin)
out = []
for case in cases:
    provider = case["provider"]
    model = case["model"]
    body = _prepare_model_request(case["config"], provider, model, case["route"], case["body"])
    out.append({
        "body": body,
        "policy": model.get("_emp_reasoning_summary_policy"),
        "preserve": model.get("_emp_preserve_reasoning_summary"),
        "state": model.get("_emp_preserve_reasoning_state"),
        "future_model_field": model.get("future_model_field"),
    })
json.dump(out, sys.stdout, separators=(",", ":"))
"#;
        let mut child = Command::new(python)
            .arg("-c")
            .arg(script)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn Python 0.11.10 reasoning-summary oracle");
        child
            .stdin
            .take()
            .expect("Python stdin")
            .write_all(serde_json::to_string(cases).unwrap().as_bytes())
            .expect("write oracle cases");
        let output = child.wait_with_output().expect("wait for Python oracle");
        assert!(
            output.status.success(),
            "Python reasoning-summary oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(serde_json::from_slice(&output.stdout).expect("Python result JSON"))
    }

    #[test]
    fn external_request_preparation_matches_live_python_table() {
        let cases = cases();
        let rust = rust_results(&cases);
        if let Some(python) = python_results(&cases) {
            assert_eq!(rust, python);
        }

        for (fixture, result) in cases.iter().zip(rust.as_array().unwrap()) {
            let name = fixture["name"].as_str().unwrap();
            let body = &result["body"];
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
                    assert_eq!(body, &fixture["body"]);
                    assert!(result["policy"].is_null());
                    assert!(result["preserve"].is_null());
                    assert!(result["state"].is_null());
                }
                _ => assert!(body["reasoning"].get("summary").is_none(), "{name}"),
            }
            if name == "native-unchanged" {
                assert!(result["policy"].is_null());
            } else {
                assert_eq!(result["policy"], expected_policy, "{name}");
            }
            assert_eq!(body["future_body_field"], json!({"opaque":true}));
            assert_eq!(result["future_model_field"], json!({"opaque":true}));
            if name != "native-unchanged" {
                assert_eq!(result["preserve"], expected_preserve, "{name}");
                assert_eq!(result["state"], false, "{name}");
            }
        }
    }
}
