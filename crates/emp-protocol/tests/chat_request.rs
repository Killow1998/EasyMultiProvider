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

#[test]
fn numeric_reasoning_effort_is_rejected_instead_of_silently_dropped() {
    let error = responses_to_chat(&json!({"reasoning":{"effort":64}}), "upstream")
        .expect_err("Chat Completions does not accept numeric reasoning effort");
    assert_eq!(error.kind(), ProtocolErrorKind::InvalidRequest);
    assert!(error.to_string().contains("numeric reasoning effort"));
}

#[test]
fn multimodal_tool_results_keep_pairing_and_content_order() {
    for kind in ["function_call", "custom_tool_call"] {
        let output_kind = format!("{kind}_output");
        let body = json!({"input": [
            {"type":kind,"call_id":"first","name":"view","arguments":"{}","input":"{}"},
            {"type":kind,"call_id":"second","name":"read","arguments":"{}","input":"{}"},
            {"type":output_kind,"call_id":"first","output":[
                {"type":"input_text","text":"before"},
                {"type":"input_image","image_url":"https://example.invalid/tool.png","detail":"high"},
                {"type":"input_text","text":"after"}
            ]},
            {"type":"reasoning","encrypted_content":"not forwarded"},
            {"type":output_kind,"call_id":"second","output":"second result"},
            {"role":"assistant","content":"answer"}
        ]});
        let original = body.clone();
        let projected = responses_to_chat(&body, "upstream").unwrap();
        assert_eq!(body, original);
        let messages = projected["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|m| m["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["assistant", "tool", "tool", "user", "assistant"]
        );
        assert_eq!(messages[1]["tool_call_id"], "first");
        assert_eq!(messages[2]["tool_call_id"], "second");
        let parts = &messages[3]["content"];
        assert!(parts[0]["text"].as_str().unwrap().contains("first"));
        assert_eq!(parts[1]["text"], "before");
        assert_eq!(
            parts[2],
            json!({"type":"image_url","image_url":{"url":"https://example.invalid/tool.png","detail":"high"}})
        );
        assert_eq!(parts[3]["text"], "after");
        assert!(parts[4]["text"].as_str().unwrap().contains("second"));
        assert_eq!(parts[5]["text"], "second result");
        assert_eq!(messages[4]["content"], "answer");
    }
}

#[test]
fn standalone_tool_images_remain_visible_without_an_orphaned_tool_message() {
    let projected = responses_to_chat(
        &json!({"input":[
            {"type":"function_call_output","name":"view","namespace":"tools","output":[
                {"type":"input_text","text":"image result"},
                {"type":"input_image","image_url":"https://example.invalid/tool.png"}
            ]}
        ]}),
        "upstream",
    )
    .unwrap();
    let message = &projected["messages"][0];
    assert_eq!(message["role"], "user");
    assert!(
        message["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("tools/view")
    );
    assert_eq!(message["content"][1]["text"], "image result");
    assert_eq!(
        message["content"][2]["image_url"]["url"],
        "https://example.invalid/tool.png"
    );
}
