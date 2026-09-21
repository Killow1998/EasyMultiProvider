use emp_protocol::{
    ChatFrame, ChatIds, ChatStream, ProtocolErrorKind, StreamEvent, response_from_chat,
};
use serde_json::{Value, json};

fn ids() -> ChatIds {
    ChatIds::new("resp_fixture", "msg_fixture", "rs_leading", "rs_late").expect("valid fixture IDs")
}

fn event_types(events: &[StreamEvent]) -> Vec<&str> {
    events.iter().map(|event| event.event).collect()
}

fn terminal_response(events: &[StreamEvent]) -> &Value {
    events
        .last()
        .and_then(|event| event.value.get("response"))
        .expect("terminal response")
}

#[test]
fn complete_projection_separates_reasoning_answer_refusal_and_usage() {
    let upstream = json!({
        "provider_trace": {"kept": true},
        "choices": [{
            "message": {
                "reasoning_content": "Check the sum.",
                "reasoning": "lower precedence",
                "content": "Four.",
                "refusal": "Boundary note.",
                "future_message_field": 7
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 120,
            "completion_tokens": 30,
            "total_tokens": 150,
            "prompt_tokens_details": {"cached_tokens": 80},
            "completion_tokens_details": {"reasoning_tokens": 20}
        }
    });

    let projected = response_from_chat(&upstream, "test/model", &[], &ids()).expect("projection");
    let output = projected.response["output"]
        .as_array()
        .expect("output array");
    assert_eq!(
        output
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["reasoning", "message"]
    );
    assert_eq!(output[0]["id"], "rs_leading");
    assert_eq!(output[0]["content"][0]["text"], "Check the sum.");
    assert_eq!(output[1]["content"][0]["text"], "Four.");
    assert_eq!(output[1]["content"][1]["refusal"], "Boundary note.");
    assert_eq!(projected.response["output_text"], "Four.");
    assert_eq!(
        projected.response["usage"],
        json!({
            "input_tokens": 120,
            "output_tokens": 30,
            "total_tokens": 150,
            "input_tokens_details": {"cached_tokens": 80},
            "output_tokens_details": {"reasoning_tokens": 20}
        })
    );
    assert!(
        projected
            .unknown_fields
            .iter()
            .any(|field| field.path == "$.provider_trace")
    );
    assert!(
        projected
            .unknown_fields
            .iter()
            .any(|field| field.path == "$.choices[0].message.future_message_field")
    );
    assert!(!format!("{:?}", projected.unknown_fields).contains("kept"));
}

#[test]
fn complete_projection_requires_object_tool_arguments_and_finish_reason() {
    for bad in [
        json!({"choices": [{"message": {"content": "answer"}}]}),
        json!({"choices": [{"message": {"tool_calls": "bad"}, "finish_reason": "stop"}]}),
        json!({"choices": [{"message": {"tool_calls": [{
            "id": "call_bad", "function": {"name": "tool", "arguments": "[]"}
        }]}, "finish_reason": "tool_calls"}]}),
    ] {
        assert!(response_from_chat(&bad, "test/model", &[], &ids()).is_err());
    }

    let projected = response_from_chat(
        &json!({"choices": [{"message": {"tool_calls": [{
            "id": "call_custom",
            "function": {"name": "shell", "arguments": "{\"input\":\"pwd\"}"},
            "extra_content": {"provider": "value"}
        }]}, "finish_reason": "tool_calls"}]}),
        "test/model",
        &["shell"],
        &ids(),
    )
    .expect("custom call");
    let call = &projected.response["output"][0];
    assert_eq!(call["type"], "custom_tool_call");
    assert_eq!(call["call_id"], "call_custom");
    assert_eq!(call["input"], "pwd");
    assert_eq!(call["id"], "ctc_62f939c22b98e67f872e1dac");
    assert_eq!(call["extra_content"], json!({"provider": "value"}));
}

#[test]
fn stream_keeps_reasoning_and_answer_in_distinct_ordered_items() {
    let mut stream = ChatStream::new("test/model", ids());
    let mut events = vec![stream.start_event().expect("start")];
    for chunk in [
        json!({"choices": [{"delta": {"content": null, "reasoning_content": "Check "}}]}),
        json!({"choices": [{"delta": {"reasoning_content": "the sum.", "content": "Four."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {
            "prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5
        }}),
    ] {
        events.extend(stream.push(&chunk, ChatFrame::Sse).expect("chunk"));
    }
    stream.mark_done();
    events.extend(stream.finish().expect("finish"));

    let reasoning = events
        .iter()
        .filter(|event| event.event == "response.reasoning_text.delta")
        .map(|event| event.value["delta"].as_str().unwrap())
        .collect::<String>();
    let answer = events
        .iter()
        .filter(|event| event.event == "response.output_text.delta")
        .map(|event| event.value["delta"].as_str().unwrap())
        .collect::<String>();
    assert_eq!(reasoning, "Check the sum.");
    assert_eq!(answer, "Four.");

    let item_lifecycle = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                "response.output_item.added" | "response.output_item.done"
            )
        })
        .map(|event| (event.event, event.value["item"]["type"].as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        item_lifecycle,
        [
            ("response.output_item.added", "reasoning"),
            ("response.output_item.done", "reasoning"),
            ("response.output_item.added", "message"),
            ("response.output_item.done", "message"),
        ]
    );
    let part_added = events
        .iter()
        .find(|event| event.event == "response.content_part.added")
        .expect("content part");
    assert_eq!(part_added.value["part"]["text"], "");
    let response = terminal_response(&events);
    assert_eq!(response["output_text"], "Four.");
    assert_eq!(response["usage"]["input_tokens"], 2);
    assert_eq!(event_types(&events).last(), Some(&"response.completed"));
    assert!(stream.finish().is_err(), "a terminal event is emitted once");
}

#[test]
fn late_reasoning_uses_its_own_id_and_output_index() {
    let mut stream = ChatStream::new("test/model", ids());
    stream.start_event().expect("start");
    stream
        .push(
            &json!({"choices": [{"delta": {"content": "First."}}]}),
            ChatFrame::Sse,
        )
        .expect("answer");
    stream
        .push(
            &json!({"choices": [{"delta": {"reasoning_text": "Late."}, "finish_reason": "stop"}]}),
            ChatFrame::Sse,
        )
        .expect("late reasoning");
    stream.mark_done();
    let events = stream.finish().expect("finish");
    let delta = events
        .iter()
        .find(|event| event.event == "response.reasoning_text.delta")
        .expect("late delta");
    assert_eq!(delta.value["item_id"], "rs_late");
    assert_eq!(delta.value["output_index"], 1);
    let response = terminal_response(&events);
    assert_eq!(response["output"][0]["type"], "message");
    assert_eq!(response["output"][1]["type"], "reasoning");
}

#[test]
fn message_closes_before_tool_and_tool_frames_match_responses() {
    let mut stream = ChatStream::new("test/model", ids());
    stream.start_event().expect("start");
    stream
        .push(
            &json!({"choices": [{"delta": {"content": "Calculating."}}]}),
            ChatFrame::Sse,
        )
        .expect("answer");
    stream
        .push(
            &json!({"choices": [{"delta": {"tool_calls": [{
                "index": "0", "id": "call_fixture", "type": "function",
                "function": {"name": "calculator", "arguments": "{\"x\":"},
                "extra_content": {"vendor": true}
            }]}}]}),
            ChatFrame::Sse,
        )
        .expect("tool start");
    stream
        .push(
            &json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "function": {"arguments": "4}"}
            }]}, "finish_reason": "tool_calls"}]}),
            ChatFrame::Sse,
        )
        .expect("tool finish");
    stream.mark_done();
    let events = stream.finish().expect("finish");
    let lifecycle = events
        .iter()
        .filter(|event| {
            matches!(
                event.event,
                "response.output_item.added" | "response.output_item.done"
            )
        })
        .map(|event| (event.event, event.value["item"]["type"].as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        [
            ("response.output_item.done", "message"),
            ("response.output_item.added", "function_call"),
            ("response.output_item.done", "function_call"),
        ]
    );
    let added = events
        .iter()
        .find(|event| {
            event.event == "response.output_item.added"
                && event.value["item"]["type"] == "function_call"
        })
        .expect("tool added");
    assert_eq!(
        added.value["item"]["extra_content"],
        json!({"vendor": true})
    );
    let done = events
        .iter()
        .find(|event| event.event == "response.function_call_arguments.done")
        .expect("arguments done");
    assert_eq!(done.value["arguments"], "{\"x\":4}");
    assert!(done.value.get("delta").is_none());
    assert!(done.value.get("content_index").is_none());
}

#[test]
fn refusal_only_stream_is_output_and_usage_only_tail_is_valid() {
    let mut stream = ChatStream::new("test/model", ids());
    stream.start_event().expect("start");
    stream
        .push(
            &json!({"choices": [{"delta": {"refusal": "No."}, "finish_reason": "stop"}]}),
            ChatFrame::Sse,
        )
        .expect("refusal");
    stream
        .push(
            &json!({"choices": [], "usage": {"completion_tokens": 1}}),
            ChatFrame::Sse,
        )
        .expect("usage tail");
    stream.mark_done();
    let events = stream.finish().expect("finish refusal");
    let response = terminal_response(&events);
    assert_eq!(
        response["output"][0]["content"][0],
        json!({"type": "refusal", "refusal": "No."})
    );
    assert_eq!(response["usage"]["output_tokens"], 1);
}

#[test]
fn literal_markup_is_text_and_missing_boundaries_fail_closed() {
    let literal = "Example: <think>text</think> and <tool_call> is documentation.";
    let mut stream = ChatStream::new("test/model", ids());
    stream.start_event().expect("start");
    stream
        .push(
            &json!({"choices": [{"delta": {"content": literal}, "finish_reason": "stop"}]}),
            ChatFrame::Sse,
        )
        .expect("literal");
    stream.mark_done();
    assert_eq!(
        terminal_response(&stream.finish().expect("finish"))["output_text"],
        literal
    );

    let mut missing_done = ChatStream::new("test/model", ids());
    missing_done.start_event().expect("start");
    missing_done
        .push(
            &json!({"choices": [{"delta": {"content": "partial"}, "finish_reason": "stop"}]}),
            ChatFrame::Sse,
        )
        .expect("chunk");
    assert_eq!(
        missing_done.finish().expect_err("missing DONE").kind(),
        ProtocolErrorKind::StreamIncomplete
    );

    let mut missing_reason = ChatStream::new("test/model", ids());
    missing_reason.start_event().expect("start");
    missing_reason
        .push(
            &json!({"choices": [{"delta": {"content": "answer"}}]}),
            ChatFrame::Sse,
        )
        .expect("chunk");
    missing_reason.mark_done();
    assert!(missing_reason.finish().is_err());
}

#[test]
fn errors_and_debug_output_do_not_echo_upstream_secrets() {
    let secret = "sk-secret-must-not-appear";
    let error = response_from_chat(
        &json!({"error": {"message": secret}}),
        "test/model",
        &[],
        &ids(),
    )
    .expect_err("upstream error");
    assert!(!error.to_string().contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}
