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
fn opaque_json_with_limit_rejects_oversized_deserialized_values() {
    let raw = r#"{"future":{"values":"0123456789"}}"#;
    let value: Map<String, Value> = serde_json::from_str(raw).expect("parse raw");
    let error = OpaqueJson::with_limit(value, 8)
        .err()
        .map(|error| error.to_string())
        .expect("oversized opaque JSON should fail");
    assert!(error.contains("limit is 8 bytes"), "{error}");
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

#[test]
fn request_context_enforces_identity_and_deadline() {
    let context = RequestContext::new("0123456789abcdef", Duration::from_secs(10), true)
        .expect("valid context")
        .with_deadline(Duration::from_secs(20))
        .expect("valid deadline");
    assert!(context.stream);
    assert_eq!(context.deadline_at, Some(Duration::from_secs(20)));

    let short_id = RequestContext::new("short", Duration::ZERO, false).expect_err("invalid id");
    assert_eq!(short_id, ValidationError::InvalidRequestId);
    let backwards = RequestContext::new("0123456789abcdef", Duration::from_secs(20), false)
        .expect("valid context")
        .with_deadline(Duration::from_secs(10))
        .expect_err("invalid deadline");
    assert_eq!(backwards, ValidationError::InvalidDeadline);
}

#[test]
fn prepared_request_preserves_unknown_body_fields() {
    let route = route();
    let body = json!({
        "model": "demo/model",
        "input": "continue",
        "future_protocol_field": {"opaque": true}
    });
    let headers = BTreeMap::from([("authorization".to_string(), "Bearer fixture".to_string())]);
    let request = PreparedRequest::new(
        route.clone(),
        Method::Post,
        "https://example.test/v1/responses",
        headers,
        Some(body.as_object().expect("object").clone()),
        Map::new(),
    )
    .expect("valid request");
    assert_eq!(
        request
            .body
            .as_ref()
            .expect("body")
            .value()
            .get("future_protocol_field"),
        Some(&json!({"opaque": true}))
    );
    let restored: PreparedRequest =
        serde_json::from_str(&serde_json::to_string(&request).expect("serialize"))
            .expect("deserialize");
    assert_eq!(restored, request);
}

#[test]
fn prepared_request_requires_supported_url() {
    let error = PreparedRequest::new(
        route(),
        Method::Post,
        "gopher://example.test",
        BTreeMap::new(),
        None,
        Map::new(),
    )
    .expect_err("invalid URL");
    assert_eq!(error, ValidationError::InvalidUrl);
}

#[test]
fn public_failure_normalizes_safe_tokens() {
    let failure = PublicFailure::new(
        "Rate Limit",
        Some(429),
        Some("First Event".to_string()),
        Some("upstream capacity!!".to_string()),
        Some("The upstream rate limit was reached.".to_string()),
        Some(15),
        None,
    )
    .expect("valid failure");
    assert_eq!(failure.error_class, "rate_limit");
    assert_eq!(failure.phase.as_deref(), Some("first_event"));
    assert_eq!(failure.failure_reason.as_deref(), Some("upstream_capacity"));
    let serialized = serde_json::to_value(&failure).expect("serialize");
    assert_eq!(serialized["status"], 429);
    assert!(serialized.get("context_observation").is_none());

    let invalid = PublicFailure::new("none", Some(99), None, None, None, None, None)
        .expect_err("invalid status");
    assert_eq!(invalid, ValidationError::InvalidStatus);
}

#[test]
fn stream_observation_requires_terminal_truth() {
    let terminal = PublicFailure::new(
        "stream_incomplete",
        Some(502),
        Some("terminal_validation".to_string()),
        Some("stream incomplete".to_string()),
        Some("The upstream stream ended without a valid completion event.".to_string()),
        None,
        None,
    )
    .expect("terminal");
    let observation = StreamObservation::new(
        "terminal_validation",
        125,
        Some(20),
        0,
        true,
        false,
        true,
        true,
        Some(terminal.clone()),
        Map::new(),
    )
    .expect("observation");
    assert_eq!(observation.terminal, Some(terminal));
    let missing = StreamObservation::new(
        "terminal_validation",
        125,
        None,
        0,
        false,
        false,
        true,
        true,
        None,
        Map::new(),
    )
    .expect_err("terminal missing");
    assert_eq!(missing, ValidationError::EmptyField("terminal"));
}

#[test]
fn injected_sources_are_deterministic() {
    let clock = FixedClock(Duration::from_secs(42));
    assert_eq!(clock.now(), Duration::from_secs(42));
    assert_eq!(
        clock.deadline_after(Duration::from_secs(1)),
        Duration::from_secs(43)
    );

    let random = FixedRandomSource(0xa5);
    assert_eq!(random.request_id(), "a5a5a5a5a5a5a5a5");
    let admission = PermissiveMemoryInspector
        .admit(1024)
        .expect("permissive admission");
    assert_eq!(admission.required_bytes, 1024);
    assert_eq!(admission.available_bytes, None);
}
