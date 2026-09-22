use emp_core::{
    deployment_identity, endpoint_fingerprint, normalize_endpoint, resolve_route,
    resolved_upstream_model,
};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/core/python-route-resolution.json"
    ))
    .expect("valid route fixture")
}

fn catalog_model(
    case: &Value,
    slug: &str,
    account: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    let key = account
        .and_then(|account| account.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("_native");
    case.get("catalog")?
        .get(key)?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|model| {
            model.get("slug").and_then(Value::as_str) == Some(slug)
                && model.get("supported_in_api").and_then(Value::as_bool) != Some(false)
        })
        .cloned()
}

fn rust_result(fixture: &Value) -> Value {
    let endpoints = fixture["endpoints"]
        .as_array()
        .expect("endpoint cases")
        .iter()
        .map(|endpoint| {
            let endpoint = endpoint.as_str().expect("endpoint string");
            json!({
                "normalized": normalize_endpoint(Some(endpoint)),
                "fingerprint": endpoint_fingerprint(Some(endpoint)),
            })
        })
        .collect::<Vec<_>>();
    let identities = fixture["identities"]
        .as_array()
        .expect("identity cases")
        .iter()
        .map(|case| {
            let provider = case["provider"].as_object().expect("provider object");
            let model = case["model"].as_object().expect("model object");
            Value::String(deployment_identity(provider, model))
        })
        .collect::<Vec<_>>();
    let upstreams = fixture["upstreams"]
        .as_array()
        .expect("upstream cases")
        .iter()
        .map(|case| {
            Value::String(resolved_upstream_model(
                case["provider"].as_object().expect("provider object"),
                case["model"].as_object().expect("model object"),
                case["requested"].as_str().expect("requested model"),
            ))
        })
        .collect::<Vec<_>>();
    let routes = fixture["routes"]
        .as_array()
        .expect("route cases")
        .iter()
        .map(|case| {
            match resolve_route(
                &case["config"],
                case["model_id"].as_str().expect("model id"),
                |_, slug, account| catalog_model(case, slug, account),
            ) {
                Ok(route) => json!({
                    "ok": true,
                    "requested_model": route.requested_model,
                    "upstream_model": route.upstream_model,
                    "source": route.source,
                    "provider": route.provider.value(),
                    "model": route.model.value(),
                    "protocol": route.protocol,
                    "dialect": route.dialect,
                    "provider_id": route.provider_id,
                    "endpoint_fingerprint": route.endpoint_fingerprint,
                    "deployment_identity": route.deployment_identity,
                }),
                Err(error) => json!({
                    "ok": false,
                    "status": error.status(),
                    "message": error.to_string(),
                }),
            }
        })
        .collect::<Vec<_>>();
    json!({
        "endpoints": endpoints,
        "identities": identities,
        "upstreams": upstreams,
        "routes": routes,
    })
}

#[test]
fn route_resolution_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixture();
    let script = r#"
import copy, json, sys
from unittest.mock import patch
from easy_multi_provider.capabilities import deployment_identity, endpoint_fingerprint, normalize_endpoint
from easy_multi_provider.router_errors import RouterError
from easy_multi_provider import route_plan

fixture = json.load(sys.stdin)

def catalog_model(case, slug, account=None):
    key = account.get("id") if isinstance(account, dict) else "_native"
    for model in case.get("catalog", {}).get(key, []):
        if model.get("slug") == slug and model.get("supported_in_api", True) is not False:
            return copy.deepcopy(model)
    return None

routes = []
for case in fixture["routes"]:
    with patch.object(route_plan, "subscription_route_model", side_effect=lambda config, slug, account=None, case=case: catalog_model(case, slug, account)):
        try:
            route = route_plan.resolve_route(case["config"], case["model_id"])
        except RouterError as error:
            routes.append({"ok": False, "status": error.status, "message": str(error)})
        else:
            routes.append({
                "ok": True,
                "requested_model": route.requested_model,
                "upstream_model": route.upstream_model,
                "source": route.source,
                "provider": route.provider_copy(),
                "model": route.model_copy(),
                "protocol": route.protocol,
                "dialect": route.dialect,
                "provider_id": route.provider_id,
                "endpoint_fingerprint": route.endpoint_fingerprint,
                "deployment_identity": route.deployment_identity,
            })

json.dump({
    "endpoints": [{
        "normalized": normalize_endpoint(value),
        "fingerprint": endpoint_fingerprint(value),
    } for value in fixture["endpoints"]],
    "identities": [deployment_identity(case["provider"], case["model"]) for case in fixture["identities"]],
    "upstreams": [route_plan.resolved_upstream_model(case["provider"], case["model"], case["requested"]) for case in fixture["upstreams"]],
    "routes": routes,
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
        .expect("spawn configured Python route oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write route fixture");
    let output = child.wait_with_output().expect("wait for route oracle");
    assert!(
        output.status.success(),
        "Python route oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    assert_eq!(rust_result(&fixture), python);
}

#[test]
fn negotiated_protocol_rebuilds_an_immutable_snapshot() {
    let fixture = fixture();
    let case = &fixture["routes"][1];
    let route = resolve_route(
        &case["config"],
        case["model_id"].as_str().expect("model id"),
        |_, slug, account| catalog_model(case, slug, account),
    )
    .expect("auto route");
    let concrete = route
        .with_protocol(emp_core::Protocol::ChatCompletions)
        .expect("concrete route");
    assert_eq!(route.protocol, emp_core::Protocol::Auto);
    assert_eq!(route.provider.value()["protocol"], "auto");
    assert_eq!(concrete.protocol, emp_core::Protocol::ChatCompletions);
    assert_eq!(concrete.provider.value()["protocol"], "chat_completions");
}
