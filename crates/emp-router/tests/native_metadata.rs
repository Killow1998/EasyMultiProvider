//! Native response metadata: which upstream headers survive into the Codex
//! session, how model identifiers are aliased back to the requested model,
//! and why credentials never pass through.

use emp_router::native_metadata::{
    native_response_headers, rewrite_native_model_event, rewrite_native_model_headers,
};
use serde_json::{Map, Value, json};

fn headers(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect(),
    )
}

#[test]
fn credential_and_hop_by_hop_headers_never_reach_the_client() {
    let response = headers(&[
        ("Authorization", json!("fixture-secret")),
        ("Set-Cookie", json!("fixture-cookie")),
        ("Content-Type", json!("application/json")),
        ("cf-ray", json!("ray-fixture")),
        ("x-request-id", json!("request-fixture")),
        ("x-codex-turn-state", json!("turn-fixture")),
        ("x-openai-model", json!("upstream-model")),
        // Rate-limit telemetry uses provider/side/metric naming; tertiary
        // sides and malformed provider segments are rejected.
        ("x-demo-primary-used-percent", json!("3")),
        ("x-demo-secondary-window-minutes", json!("5")),
        ("x-demo-limit-name", json!("fixture plan")),
        ("x-demo-tertiary-used-percent", json!("3")),
        ("x--limit-name", json!("invalid")),
        // Non-string values are dropped even for allowlisted names.
        ("x-request-id-count", json!(1)),
    ])
    .as_object()
    .expect("headers object")
    .clone();
    let selected = native_response_headers(
        &json!({"headers": response}),
        "native/model",
        "upstream-model",
    );
    assert_eq!(
        selected,
        Map::from_iter([
            ("cf-ray".to_owned(), json!("ray-fixture")),
            ("x-request-id".to_owned(), json!("request-fixture")),
            ("x-codex-turn-state".to_owned(), json!("turn-fixture")),
            ("x-openai-model".to_owned(), json!("native/model")),
            ("x-demo-primary-used-percent".to_owned(), json!("3")),
            ("x-demo-secondary-window-minutes".to_owned(), json!("5")),
            ("x-demo-limit-name".to_owned(), json!("fixture plan")),
        ])
    );
}

#[test]
fn non_object_header_payloads_yield_no_headers_instead_of_panicking() {
    for response in [Value::Null, json!([]), json!(3), json!("headers")] {
        let selected = native_response_headers(&response, "native/model", "upstream-model");
        assert!(selected.is_empty(), "{response}");
    }
    for headers in [Value::Null, json!([]), json!(7)] {
        let selected = native_response_headers(
            &json!({"headers": headers}),
            "native/model",
            "upstream-model",
        );
        assert!(selected.is_empty(), "{headers}");
    }
}

#[test]
fn upstream_model_identifiers_are_aliased_to_the_requested_model() {
    let projected = rewrite_native_model_headers(
        &headers(&[
            ("OpenAI-Model", json!("GPT-6")),
            ("X-OpenAI-Model", json!("different")),
        ]),
        "native/gpt-6",
        "gpt-6",
    );
    assert_eq!(projected["OpenAI-Model"], "native/gpt-6");
    assert_eq!(projected["X-OpenAI-Model"], "different");

    // Empty requested or upstream identifiers disable the aliasing.
    let projected = rewrite_native_model_headers(
        &headers(&[("openai-model", json!("model"))]),
        "",
        "upstream",
    );
    assert_eq!(projected["openai-model"], "model");
}

#[test]
fn model_aliasing_applies_to_event_headers_and_response_headers() {
    let event = json!({
        "response": {
            "model": "gpt-6",
            "headers": {"openai-model": "GPT-6"},
        }
    });
    let projected = rewrite_native_model_event(&event, "native/gpt-6", "gpt-6");
    assert_eq!(projected["response"]["model"], "gpt-6");
    assert_eq!(
        projected["response"]["headers"]["openai-model"],
        "native/gpt-6"
    );

    // Non-object events and non-object nested headers pass through unchanged.
    for event in [Value::Null, json!([]), json!(3), json!("headers")] {
        assert_eq!(rewrite_native_model_event(&event, "a", "b"), event);
    }
    assert_eq!(
        rewrite_native_model_event(&json!({"headers": null}), "a", "b"),
        json!({"headers": null})
    );
}
