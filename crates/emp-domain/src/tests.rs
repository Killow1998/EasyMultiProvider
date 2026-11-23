use super::*;
use serde_json::json;

fn provider_object() -> Map<String, Value> {
    let value = json!({
        "id": "demo",
        "base_url": "https://example.test/v1",
        "protocol": "responses",
        "future_provider_field": {"unknown": true}
    });
    value.as_object().expect("object").clone()
}

fn model_object() -> Map<String, Value> {
    let value = json!({
        "id": "demo/model",
        "upstream_id": "model",
        "future_model_field": [1, 2, 3]
    });
    value.as_object().expect("object").clone()
}

fn route() -> ResolvedRoute {
    ResolvedRoute::new(
        "demo/model",
        "model",
        RouteSource::ExplicitModel,
        provider_object(),
        model_object(),
        Protocol::Responses,
        Dialect::PortableResponses,
        "demo",
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        "default",
    )
    .expect("valid route")
}

#[test]
fn opaque_json_preserves_unknown_fields() {
    let raw = json!({"type": "response.create", "future": {"nested": [1, null, "x"]}});
    let value = raw.as_object().expect("object").clone();
    let opaque = OpaqueJson::new(value).expect("valid");
    let restored = serde_json::to_value(&opaque).expect("serialize");
    assert_eq!(restored, raw);
}

#[test]
fn opaque_json_enforces_its_limit() {
    let value = json!({"large": "0123456789"});
    let error = OpaqueJson::with_limit(value.as_object().expect("object").clone(), 8)
        .expect_err("too large");
    assert_eq!(error.reason, OpaqueJsonErrorReason::TooLarge);
}

#[test]
fn resolved_route_round_trips_unknown_snapshots() {
    let route = route();
    let serialized = serde_json::to_string(&route).expect("serialize");
    assert!(serialized.contains("\"future_provider_field\":{\"unknown\":true}"));
    let restored: ResolvedRoute = serde_json::from_str(&serialized).expect("deserialize");
    assert_eq!(restored, route);
    assert_eq!(
        restored.provider.value().get("future_provider_field"),
        Some(&json!({"unknown": true}))
    );
    assert_eq!(
        restored.model.value().get("future_model_field"),
        Some(&json!([1, 2, 3]))
    );
    assert_eq!(restored.source, RouteSource::ExplicitModel);
    assert_eq!(restored.dialect, Dialect::PortableResponses);
}

#[test]
fn opaque_json_deserialization_rejects_non_objects_and_preserves_fields() {
    assert!(serde_json::from_str::<OpaqueJson>("[]").is_err());
    assert!(serde_json::from_str::<OpaqueJson>("42").is_err());
    let valid: OpaqueJson =
        serde_json::from_str(r#"{"future":{"unknown":true}}"#).expect("valid JSON");
    assert_eq!(valid.value().get("future"), Some(&json!({"unknown": true})));
}

#[test]
fn resolved_route_rejects_invalid_fingerprint() {
    let error = ResolvedRoute::new(
        "demo/model",
        "model",
        RouteSource::ExplicitModel,
        provider_object(),
        model_object(),
        Protocol::Responses,
        Dialect::PortableResponses,
        "demo",
        "endpoint",
        "default",
    )
    .expect_err("invalid fingerprint");
    assert_eq!(error, ValidationError::InvalidFingerprint);
}
