//! Collaboration transport: plaintext tool metadata crossing to native
//! Responses providers and coming back to Codex.

use emp_protocol::collaboration::{
    CollaborationError, collaboration_summary, prepare_collaboration, restore_collaboration,
};
use serde_json::{Value, json};

fn body() -> serde_json::Map<String, Value> {
    serde_json::from_value(json!({
        "model": "work/gpt-5",
        "tools": [{"type": "namespace", "name": "collaboration", "tools": [
            {"type": "function", "name": "spawn_agent", "parameters": {
                "properties": {"message": {"type": "string", "encrypted": true}}
            }}
        ]}]
    }))
    .expect("request body")
}

#[test]
fn collaboration_tools_are_renamed_and_message_encryption_flag_removed() {
    let (projected, changed) = prepare_collaboration(&body()).expect("projection");
    assert!(changed);
    let tool = &projected["tools"][0];
    assert_eq!(tool["name"], "emp_collaboration");
    assert_eq!(
        tool["tools"][0]["parameters"]["properties"]["message"]["encrypted"],
        json!(null)
    );
    assert!(
        projected["tools"][0]["tools"][0]["parameters"]["properties"]["message"]
            .get("encrypted")
            .is_none()
    );

    // Diagnostics stay content-free and count both tool namespaces.
    let summary = collaboration_summary(projected.as_object().unwrap());
    assert_eq!(summary, json!({"native": 0, "emp": 1, "emp_in_history": 0}));
}

#[test]
fn round_trip_restores_codex_facing_names_and_rejects_encrypted_arguments() {
    prepare_collaboration(&body()).expect("projection");
    let call = json!({
        "type": "function_call",
        "call_id": "call_1",
        "name": "spawn_agent",
        "namespace": "emp_collaboration",
        "arguments": "{}"
    });
    let restored = restore_collaboration(&call).expect("restore");
    assert_eq!(restored["namespace"], "collaboration");
    assert_eq!(restored["encrypted_function_args"], json!([]));

    // A provider that invents ciphertext for a plaintext tool fails closed.
    let smuggled = json!({
        "type": "function_call",
        "call_id": "call_1",
        "name": "spawn_agent",
        "namespace": "emp_collaboration",
        "arguments": "{}",
        "encrypted_function_args": ["payload"]
    });
    let error = restore_collaboration(&smuggled).expect_err("encrypted arguments");
    assert_eq!(error, CollaborationError::UnexpectedEncryptedArguments);
    assert_eq!(error.status(), 400);
}

#[test]
fn history_tools_reuse_the_same_reserved_namespace_check() {
    let mut with_history = body();
    with_history.insert(
        "input".to_owned(),
        json!([{"type": "additional_tools", "id": "at_1", "tools": [
            {"type": "namespace", "name": "emp_collaboration"}
        ]}]),
    );
    let error = prepare_collaboration(&with_history).expect_err("namespace collision");
    assert_eq!(error, CollaborationError::NamespaceCollision);
    assert_eq!(error.status(), 422);
    assert_eq!(
        error.failure_reason(),
        Some("collaboration_namespace_collision")
    );

    let summary = collaboration_summary(&with_history);
    assert_eq!(summary["emp"], 1);
    assert_eq!(summary["emp_in_history"], 1);
}
