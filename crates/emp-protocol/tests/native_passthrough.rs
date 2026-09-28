//! Native passthrough: users on native Responses providers keep opaque state,
//! unknown fields, and stateful IDs; only foreign tool IDs and EMP compaction
//! summaries are rewritten.

use emp_protocol::native_responses::{NativeProjectionError, project_request};
use serde_json::json;

fn project(body: serde_json::Value) -> serde_json::Value {
    project_request(body.as_object().expect("request object")).expect("native projection")
}

#[test]
fn opaque_state_and_unknown_fields_survive_native_passthrough() {
    let body = json!({
        "model": "work/gpt-5",
        "previous_response_id": "resp_native",
        "store": true,
        "vendor_extension": {"keep": true},
        "input": [
            {"type": "reasoning", "text": "plaintext", "summary": [
                {"type": "summary_text", "text": "visible"}
            ]},
            {"type": "reasoning", "encrypted_content": "opaque", "content": [], "id": "rs_native"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
        ]
    });
    let projected = project(body);
    assert_eq!(projected["previous_response_id"], "resp_native");
    assert_eq!(projected["store"], true);
    assert_eq!(projected["vendor_extension"]["keep"], true);
    // Plaintext reasoning is dropped; the opaque item survives untouched.
    assert_eq!(projected["input"].as_array().map(Vec::len), Some(2));
    assert_eq!(projected["input"][0]["id"], "rs_native");
    assert_eq!(projected["input"][0]["encrypted_content"], "opaque");
}

#[test]
fn foreign_tool_ids_are_stripped_only_when_the_pair_is_complete() {
    let body = json!({"input": [
        {"type": "function_call", "id": "foreign", "call_id": "pair", "name": "lookup", "arguments": "{}"},
        {"type": "function_call_output", "id": "foreign_output", "call_id": "pair", "output": "result"}
    ]});
    let projected = project(body);
    assert!(projected["input"][0].get("id").is_none());
    assert!(projected["input"][1].get("id").is_none());
    assert_eq!(projected["input"][0]["call_id"], "pair");

    // An unpaired foreign call cannot be made stateless; fail closed.
    let unpaired = json!({"input": [
        {"type": "function_call", "id": "orphan", "call_id": "missing", "name": "lookup", "arguments": "{}"}
    ]});
    let error =
        project_request(unpaired.as_object().unwrap()).expect_err("unpaired foreign tool call");
    assert!(matches!(
        &error,
        NativeProjectionError::Projection(inner) if inner.failure_class() == "incompatible_tool_history"
    ));
    // Hashable checks come first: a non-string type field is a hard 500
    // boundary, not a retryable protocol error.
    let unhashable = json!({"input": [{"type": {"nested": true}}]});
    assert!(matches!(
        project_request(unhashable.as_object().unwrap()),
        Err(NativeProjectionError::UnhashableItemType)
    ));
}

#[test]
fn emp_compaction_summaries_are_decoded_into_plaintext_user_messages() {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::URL_SAFE.encode("continue from here");
    let body = json!({"input": [
        {"type": "compaction", "encrypted_content": format!("emp1:{encoded}")}
    ]});
    let projected = project(body);
    assert_eq!(projected["input"][0]["type"], "message");
    assert_eq!(projected["input"][0]["role"], "user");
    assert!(
        projected["input"][0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.ends_with("continue from here"))
    );
}
