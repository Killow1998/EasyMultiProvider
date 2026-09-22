use emp_protocol::StreamEvent;
use emp_protocol::anthropic_projection::{AnthropicErrorKind, AnthropicIds, AnthropicStream};
use serde_json::json;

fn stream(custom_names: &[&str]) -> AnthropicStream {
    AnthropicStream::new(
        "external/model",
        AnthropicIds::new("resp_fixture", "msg_fixture"),
        custom_names,
    )
}

fn types(events: &[StreamEvent]) -> Vec<&str> {
    events.iter().map(|event| event.event).collect()
}

#[test]
fn mixed_text_function_custom_and_reasoning_blocks_keep_wire_order() {
    let mut stream = stream(&["render"]);
    let mut events = vec![stream.start_event().unwrap()];
    for event in [
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "before"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "thinking", "thinking": "private"}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "thinking_delta", "thinking": "private"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "call_lookup", "name": "lookup", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"EMP\"}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "call_render", "name": "render", "input": {}}}),
        json!({"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{\"input\":\"draw\"}"}}),
        json!({"type": "content_block_stop", "index": 3}),
        json!({"type": "content_block_start", "index": 4, "content_block": {"type": "text", "text": "after"}}),
        json!({"type": "content_block_stop", "index": 4}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        json!({"type": "message_stop"}),
    ] {
        events.extend(stream.push(&event).unwrap());
    }
    events.extend(stream.finish().unwrap());

    let terminal = &events.last().unwrap().value["response"];
    assert_eq!(
        terminal["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["message", "function_call", "custom_tool_call", "message"]
    );
    assert_eq!(terminal["output_text"], "beforeafter");
    assert_eq!(terminal["output"][1]["arguments"], "{\"q\":\"EMP\"}");
    assert_eq!(terminal["output"][2]["input"], "draw");
    assert_eq!(events.last().unwrap().event, "response.completed");
    assert!(!format!("{events:?}").contains("private"));
}

#[test]
fn implicit_text_usage_and_incomplete_terminal_match_anthropic_semantics() {
    let mut stream = stream(&[]);
    let mut events = vec![stream.start_event().unwrap()];
    for event in [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 3, "output_tokens": 1}}}),
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "partial"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ] {
        events.extend(stream.push(&event).unwrap());
    }
    events.extend(stream.finish().unwrap());
    let terminal = &events.last().unwrap().value["response"];
    assert_eq!(events.last().unwrap().event, "response.incomplete");
    assert_eq!(
        terminal["incomplete_details"]["reason"],
        "max_output_tokens"
    );
    assert_eq!(terminal["usage"]["input_tokens"], 123);
    assert_eq!(terminal["usage"]["output_tokens"], 7);
    assert_eq!(terminal["usage"]["total_tokens"], 130);
    assert_eq!(terminal["output"][0]["content"][0]["text"], "partial");
    assert_eq!(
        types(&events),
        [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.incomplete"
        ]
    );
}

#[test]
fn terminal_truth_and_tool_json_fail_closed_with_distinct_error_classes() {
    let mut missing_stop = stream(&[]);
    missing_stop.start_event().unwrap();
    missing_stop
        .push(&json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "partial"}}))
        .unwrap();
    let error = missing_stop.finish().unwrap_err();
    assert_eq!(error.kind, AnthropicErrorKind::StreamIncomplete);
    assert_eq!(error.status(), 502);
    assert_eq!(error.error_class(), "stream_incomplete");

    let mut malformed = stream(&[]);
    malformed.start_event().unwrap();
    malformed
        .push(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_bad", "name": "lookup", "input": {}}}))
        .unwrap();
    malformed
        .push(&json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}))
        .unwrap();
    let error = malformed
        .push(&json!({"type": "content_block_stop", "index": 0}))
        .unwrap_err();
    assert_eq!(error.kind, AnthropicErrorKind::Upstream);
    assert_eq!(error.error_class(), "protocol_error");
}

#[test]
fn explicit_open_blocks_and_empty_suppressed_output_are_incomplete() {
    for upstream in [
        vec![
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "open"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ],
        vec![
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "redacted_thinking", "data": "private"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ],
    ] {
        let mut stream = stream(&[]);
        stream.start_event().unwrap();
        for event in upstream {
            stream.push(&event).unwrap();
        }
        assert_eq!(
            stream.finish().unwrap_err().kind,
            AnthropicErrorKind::StreamIncomplete
        );
    }
}
