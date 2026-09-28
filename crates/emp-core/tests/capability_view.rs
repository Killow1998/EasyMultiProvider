//! Capability records shown in the web UI: users must see what a model can
//! do without exposing provider credentials or trusting unverified claims.

use emp_core::capability_view::capability_record;
use serde_json::{Value, json};

fn provider(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("provider object").clone()
}

fn model(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("model object").clone()
}

#[test]
fn capability_record_hides_unknown_capabilities_and_keeps_endpoint_secret() {
    let record = capability_record(
        &provider(
            json!({"id": "demo", "base_url": "https://api.demo.example/v1",
            "protocol": "chat_completions"}),
        ),
        &model(json!({"id": "demo/gpt-5", "upstream_id": "gpt-5",
            "streaming": true, "context_window": 128000})),
    );
    let key = &record["key"];
    assert!(
        key["endpoint_fingerprint"]
            .as_str()
            .is_some_and(|fingerprint| fingerprint.starts_with("sha256:"))
    );
    assert!(!record.to_string().contains("api.demo.example"));
    assert_eq!(key["upstream_model"], "gpt-5");
    assert_eq!(record["capabilities"]["streaming"]["value"], true);
    assert_eq!(record["capabilities"]["streaming"]["source"], "inferred");
    assert_eq!(record["capabilities"]["context_window"]["value"], 128000);
    // Unknown capabilities must read as unknown, not invented booleans.
    assert_eq!(record["capabilities"]["websocket"]["source"], "unknown");
    assert_eq!(record["capabilities"]["websocket"]["value"], "unknown");
}

#[test]
fn observed_protocol_only_counts_when_the_observation_matches_the_route() {
    let base = json!({"id": "demo", "base_url": "https://api.demo.example/v1",
        "protocol": "auto"});
    let observation = json!({
        "endpoint_fingerprint": "sha256:wrong",
        "confidence": 0.9
    });

    // A mismatched fingerprint is not evidence for this endpoint.
    let mismatched = capability_record(
        &provider(base.clone()),
        &model(json!({"id": "demo/gpt-5", "resolved_protocol": "responses",
            "protocol_observation": observation})),
    );
    assert_eq!(
        mismatched["capabilities"]["effective_protocol"]["value"],
        "unknown"
    );

    // With the correct fingerprint the observation resolves the protocol.
    let fingerprint = mismatched["key"]["endpoint_fingerprint"].clone();
    let matched = capability_record(
        &provider(base),
        &model(json!({"id": "demo/gpt-5", "resolved_protocol": "responses",
            "protocol_observation": {"endpoint_fingerprint": fingerprint,
                "confidence": 0.9}})),
    );
    assert_eq!(
        matched["capabilities"]["effective_protocol"]["value"],
        "responses"
    );
    assert_eq!(matched["key"]["protocol_identity"], "responses");
}

#[test]
fn out_of_range_confidence_is_rejected_as_unknown() {
    let record = capability_record(
        &provider(json!({"id": "demo", "protocol": "chat_completions"})),
        &model(json!({"id": "demo/gpt-5", "streaming": true,
            "capability_sources": {"streaming": {"source": "official",
                "confidence": 1.5, "observed_at": null}}})),
    );
    assert_eq!(record["capabilities"]["streaming"]["source"], "unknown");
    assert_eq!(record["capabilities"]["streaming"]["confidence"], 0.0);
}
