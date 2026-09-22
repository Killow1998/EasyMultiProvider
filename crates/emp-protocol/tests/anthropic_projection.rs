use emp_protocol::anthropic_projection::{
    AnthropicErrorKind, AnthropicIds, response_from_anthropic, responses_to_anthropic,
};
use serde_json::json;

fn ids() -> AnthropicIds {
    AnthropicIds::new("resp_rust", "msg_rust")
}

#[test]
fn request_projection_preserves_messages_tools_images_and_controls() {
    let body = json!({
        "instructions": [{"type": "input_text", "text": "Be concise."}],
        "input": "Hello",
        "stream": true,
        "temperature": 0.2,
        "top_p": 0.9,
        "stop": "END",
        "max_output_tokens": 42,
        "reasoning": {"effort": "low"},
        "text": {"format": {
            "type": "json_schema", "name": "answer_schema", "strict": true,
            "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}
        }},
        "tools": [
            {"type": "function", "name": "search", "description": false, "parameters": {"type": "object"}},
            {"type": "custom", "name": "shell", "description": "Run code"}
        ],
        "tool_choice": {"type": "custom", "name": "shell"},
        "parallel_tool_calls": false
    });
    let original = body.clone();
    let projected = responses_to_anthropic(&body, "claude-upstream").expect("projection");
    assert_eq!(body, original);
    assert_eq!(projected["model"], "claude-upstream");
    assert_eq!(projected["system"], "Be concise.");
    assert_eq!(projected["max_tokens"], 42);
    assert_eq!(projected["stream"], true);
    assert_eq!(projected["stop_sequences"], json!(["END"]));
    assert_eq!(projected["output_config"]["effort"], "low");
    assert_eq!(
        projected["output_config"]["format"]["schema"]["type"],
        "object"
    );
    assert_eq!(projected["tools"][0]["input_schema"]["type"], "object");
    assert_eq!(projected["tools"][1]["name"], "shell");
    assert_eq!(projected["tool_choice"]["disable_parallel_tool_use"], true);
}

#[test]
fn request_projection_preserves_history_boundaries_and_rejects_loss() {
    let projected = responses_to_anthropic(
        &json!({
            "input": [
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Calling."},
                    {"type": "refusal", "refusal": "Boundary."}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"EMP\"}"},
                {"type": "reasoning", "encrypted_content": "opaque-not-forwarded"},
                {"type": "function_call_output", "call_id": "call_1", "output": [
                    {"type": "output_text", "text": "result"}
                ]},
                {"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [
                    {"type": "input_text", "text": "Message Type: NEW_TASK"},
                    {"type": "input_text", "text": "Do work."}
                ]},
                {"type": "function_call_output", "name": "diagnostic", "namespace": "tools", "output": "standalone"},
                {"type": "compaction", "encrypted_content": "emp1:U3VtbWFyeSB0ZXh0Lg=="}
            ]
        }),
        "claude-upstream",
    )
    .expect("history projection");
    assert_eq!(
        projected["messages"][0],
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "Calling."},
            {"type": "text", "text": "Boundary."}
        ]})
    );
    assert_eq!(
        projected["messages"][1],
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "search", "input": {"q": "EMP"}}]})
    );
    assert_eq!(
        projected["messages"][2],
        json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "result"}]})
    );
    assert_eq!(
        projected["messages"][3]["content"][0]["text"],
        "Message Type: NEW_TASK\nDo work."
    );
    assert_eq!(
        projected["messages"][4]["content"][0]["text"],
        "Standalone tool output from tools/diagnostic:\nstandalone"
    );
    assert_eq!(
        projected["messages"][5]["content"][0]["text"],
        format!(
            "Another language model started this task and produced a continuation summary. Use it to continue without repeating completed work:\n\nSummary text."
        )
    );

    for invalid in [
        json!({"input": [{"type": "message", "role": "system", "content": "system"}]}),
        json!({"input": [{"type": "message", "content": [{"type": "audio", "data": "secret"}]}]}),
        json!({"input": [{"type": "function_call", "call_id": "call", "name": "tool", "arguments": "[]"}]}),
        json!({"input": [{"type": "function_call_output", "call_id": "missing", "output": "value"}]}),
        json!({"input": [{"type": "compaction", "encrypted_content": "opaque"}]}),
        json!({"input": [{"type": "message", "content": [{"type": "input_image", "image_url": "data:image/svg+xml;base64,PHN2Zz48L3N2Zz4="}]}]}),
    ] {
        let error = responses_to_anthropic(&invalid, "claude").expect_err("lossy input rejected");
        let expected_kind = if invalid["input"][0]["type"] == "compaction" {
            AnthropicErrorKind::HistoryReconstruction
        } else {
            AnthropicErrorKind::Request
        };
        assert_eq!(error.kind, expected_kind);
        assert!(
            !error.to_string().contains("secret"),
            "failure message leaked payload"
        );
    }
}

#[test]
fn complete_response_separates_text_tools_usage_and_terminal_reason() {
    let upstream = json!({
        "error": null,
        "content": [
            {"type": "text", "text": "Partial "},
            {"type": "tool_use", "id": "call_custom", "name": "shell", "input": {"input": "pwd"}},
            {"type": "text", "text": "answer"}
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 100,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 3,
            "output_tokens": 7,
            "cache_creation": {"ephemeral_1h_input_tokens": 2}
        }
    });
    let mut projected_ids = ids();
    let projected =
        response_from_anthropic(&upstream, "test/model", &["shell"], &mut projected_ids)
            .expect("response projection");
    let output = projected["output"].as_array().expect("output array");
    assert_eq!(output.len(), 3);
    assert_eq!(output[0]["content"][0]["text"], "Partial ");
    assert_eq!(output[1]["type"], "custom_tool_call");
    assert_eq!(output[1]["call_id"], "call_custom");
    assert_eq!(output[1]["input"], "pwd");
    assert_eq!(output[2]["content"][0]["text"], "answer");
    assert_eq!(projected["output_text"], "Partial answer");
    assert_eq!(
        projected["usage"],
        json!({
            "output_tokens": 7,
            "input_tokens_details": {
                "cached_tokens": 20,
                "cache_creation_tokens": 3,
                "cache_creation_1h_tokens": 2
            },
            "input_tokens": 123,
            "total_tokens": 130
        })
    );
    assert_eq!(projected["status"], "completed");
}

#[test]
fn complete_response_maps_incomplete_and_rejects_unrepresentable_content() {
    let projected = response_from_anthropic(
        &json!({"content": [{"type": "text", "text": "partial"}], "stop_reason": "max_tokens"}),
        "test/model",
        &[],
        &mut ids(),
    )
    .expect("incomplete response");
    assert_eq!(projected["status"], "incomplete");
    assert_eq!(
        projected["incomplete_details"],
        json!({"reason": "max_output_tokens"})
    );

    for bad in [
        json!({"content": [{"type": "image", "source": {"type": "url", "url": "https://example.invalid/image"}}], "stop_reason": "end_turn"}),
        json!({"content": [{"type": "text"}], "stop_reason": "end_turn"}),
        json!({"content": [{"type": "tool_use", "id": "call", "name": "tool", "input": []}], "stop_reason": "tool_use"}),
        json!({"content": [{"type": "tool_use", "id": "call", "name": "tool", "input": {}}], "stop_reason": "pause_turn"}),
    ] {
        let error = response_from_anthropic(&bad, "test/model", &[], &mut ids())
            .expect_err("unrepresentable response rejected");
        assert_eq!(error.kind, AnthropicErrorKind::Upstream);
        assert!(
            !error.to_string().contains("https://example.invalid/image"),
            "failure message leaked upstream payload"
        );
    }
}
