use emp_protocol::{ProtocolErrorKind, responses_to_chat};
use serde_json::json;

#[test]
fn request_projection_preserves_input_and_rejects_lossy_shapes() {
    let body = json!({
        "input": [{
            "type": "message",
            "role": "user",
            "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "https://example.com/image.png", "detail": "high"}
            ]
        }],
        "tools": [{"type": "function", "name": "search", "parameters": {"type": "object"}}]
    });
    let original = body.clone();
    let projected = responses_to_chat(&body, "upstream").expect("projection");
    assert_eq!(body, original);
    assert_eq!(projected["messages"][0]["content"][0]["text"], "look");
    assert_eq!(
        projected["messages"][0]["content"][1]["image_url"]["url"],
        "https://example.com/image.png"
    );
    assert_eq!(projected["tools"][0]["function"]["name"], "search");

    for invalid in [
        json!({"input": [{"type": "message", "content": [{"type": "audio", "data": "secret"}]}]}),
        json!({"input": [{"type": "function_call", "call_id": "call", "name": "tool", "arguments": "[]"}]}),
        json!({"input": [{"type": "function_call_output", "call_id": "missing", "output": "value"}]}),
        json!({"input": [{"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [{"type": "encrypted_content", "encrypted_content": "opaque"}]}]}),
        json!({"input": [{"type": "compaction", "encrypted_content": "opaque"}]}),
    ] {
        let error = responses_to_chat(&invalid, "upstream").expect_err("lossy input rejected");
        assert_eq!(error.kind(), ProtocolErrorKind::InvalidRequest);
        assert_eq!(error.status(), 422);
        assert!(!error.to_string().contains("secret"));
        assert!(!error.to_string().contains("opaque"));
    }
}

#[test]
fn tool_pairing_and_collaboration_text_keep_turn_boundaries() {
    let projected = responses_to_chat(
        &json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":1}"},
                {"type": "reasoning", "encrypted_content": "not forwarded"},
                {"type": "function_call_output", "call_id": "call_1", "output": "result"},
                {"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [
                    {"type": "input_text", "text": "Message Type: NEW_TASK"},
                    {"type": "input_text", "text": "Do work."}
                ]}
            ]
        }),
        "upstream",
    )
    .expect("tool and agent projection");
    assert_eq!(projected["messages"][0]["role"], "assistant");
    assert_eq!(projected["messages"][0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(projected["messages"][1]["role"], "tool");
    assert_eq!(projected["messages"][1]["tool_call_id"], "call_1");
    assert_eq!(projected["messages"][2]["role"], "user");
    assert_eq!(
        projected["messages"][2]["content"],
        "Message Type: NEW_TASK\nDo work."
    );
}
