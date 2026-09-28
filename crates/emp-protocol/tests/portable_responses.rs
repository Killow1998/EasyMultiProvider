//! Portable Responses projection: user-visible behavior when EMP forwards a
//! Codex Responses request to an external stateless provider and back.

use emp_protocol::portable_responses::{
    PortableStreamProjector, custom_tool_names, project_request, project_response,
    terminal_observation, validate_responses_body,
};
use serde_json::{Value, json};

fn provider() -> serde_json::Map<String, Value> {
    serde_json::from_value(json!({
        "protocol": "responses",
        "auth_mode": "api_key",
        "base_url": "https://external.example/v1"
    }))
    .expect("provider config")
}

fn project(body: Value) -> Value {
    project_request(&provider(), &body, false).expect("portable projection")
}

fn project_result(
    body: Value,
) -> Result<Value, emp_protocol::portable_responses::PortableProjectionError> {
    project_request(&provider(), &body, false)
}

#[test]
fn request_projection_forwards_only_portable_fields_and_defaults_stream_off() {
    let projected = project(json!({
        "model": "work/gpt-5",
        "instructions": "answer briefly",
        "input": "hello",
        "temperature": 0.2,
        "metadata": {"ignored": true},
        "previous_model": "ignored"
    }));
    assert_eq!(
        projected,
        json!({
            "model": "work/gpt-5",
            "instructions": "answer briefly",
            "input": "hello",
            "temperature": 0.2,
            "stream": false
        })
    );
}

#[test]
fn stateful_requests_are_rejected_because_history_must_be_stateless() {
    let error = project_request(
        &provider(),
        &json!({"model": "work/gpt-5", "input": [], "previous_response_id": "resp_1"}),
        false,
    )
    .expect_err("previous_response_id cannot cross the boundary");
    assert_eq!(error.failure_class(), "stateful_response_unsupported");
    assert_eq!(error.index(), 0);
}

#[test]
fn plaintext_reasoning_is_stripped_but_encrypted_state_can_be_preserved() {
    let history = json!([
        {"type": "reasoning", "encrypted_content": "gAAAA", "summary": [
            {"type": "summary_text", "text": "why"}
        ]},
        {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "go"}]}
    ]);

    // Default: opaque reasoning state is dropped from the replayed history.
    let projected = project(json!({"model": "work/gpt-5", "input": history}));
    assert_eq!(
        projected["input"],
        json!([{"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "go"}]}])
    );

    // Opt-in: the encrypted blob survives so multi-turn reasoning works.
    let preserving = project_request(
        &provider(),
        &json!({"model": "work/gpt-5", "input": history}),
        true,
    )
    .expect("projection with reasoning state");
    assert_eq!(
        preserving["input"][0],
        json!({"type": "reasoning", "encrypted_content": "gAAAA"})
    );
}

#[test]
fn encrypted_agent_tasks_never_replay_without_plaintext() {
    let error = project_request(
        &provider(),
        &json!({"model": "work/gpt-5", "input": [{
            "type": "agent_message",
            "encrypted_content": "gAAAA",
            "content": []
        }]}),
        false,
    )
    .expect_err("encrypted agent task");
    assert_eq!(
        error.failure_class(),
        "encrypted_agent_task_requires_plaintext"
    );
    assert_eq!(error.item_type(), "agent_message");
}

#[test]
fn system_and_developer_messages_are_hoisted_into_instructions() {
    let projected = project(json!({
        "instructions": "base prompt",
        "input": [
            {"type": "message", "role": "system", "content": "be terse"},
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "cite sources"}]},
            {"type": "message", "role": "user", "content": "hi"}
        ]
    }));
    assert_eq!(
        projected["instructions"],
        json!("base prompt\n\nbe terse\n\ncite sources")
    );
    assert_eq!(projected["input"].as_array().map(Vec::len), Some(1));
    assert_eq!(projected["input"][0]["role"], "user");
}

#[test]
fn tool_calls_and_outputs_stay_paired_in_replayed_history() {
    let error = project_result(json!({
        "model": "work/gpt-5",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_9", "output": "42"}
        ]
    }))
    .expect_err("output without its call");
    assert_eq!(error.failure_class(), "invalid_tool_pair");

    let projected = project(json!({
        "model": "work/gpt-5",
        "input": [
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":1}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "42"}
        ]
    }));
    assert_eq!(
        projected["input"],
        json!([
            {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"q\":1}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "42"}
        ])
    );
}

#[test]
fn opaque_and_corrupt_compaction_items_are_refused() {
    let error = project_result(json!({
        "model": "work/gpt-5",
        "input": [{"type": "compaction", "encrypted_content": "not-ours"}]
    }))
    .expect_err("opaque compaction");
    assert_eq!(error.failure_class(), "opaque_compaction");

    // A real summary decodes and becomes a user message the provider can read.
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::URL_SAFE.encode("continue here");
    let projected = project(json!({
        "model": "work/gpt-5",
        "input": [{"type": "compaction", "encrypted_content": format!("emp1:{encoded}")}]
    }));
    assert!(
        projected["input"][0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.ends_with("continue here"))
    );
}

#[test]
fn custom_tools_are_downgraded_to_function_schema_for_external_providers() {
    let body = json!({
        "model": "work/gpt-5",
        "input": [],
        "tools": [{"type": "custom", "name": "run_query", "description": "run it"}],
        "tool_choice": {"type": "custom", "name": "run_query"}
    });
    assert_eq!(
        custom_tool_names(&body).expect("custom tool names"),
        ["run_query".to_owned()].into_iter().collect()
    );
    let projected = project(body);
    assert_eq!(projected["tools"][0]["type"], "function");
    assert_eq!(projected["tools"][0]["name"], "run_query");
    assert_eq!(projected["tool_choice"]["type"], "function");
}

#[test]
fn response_projection_strips_plaintext_reasoning_from_upstream_output() {
    let response = json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {"type": "reasoning", "id": "r1", "summary": [
                {"type": "summary_text", "text": "thinking hard"}
            ], "encrypted_content": "gAAAA"},
            {"type": "message", "id": "m1", "role": "assistant", "content": [
                {"type": "reasoning_text", "text": "secret chain"},
                {"type": "output_text", "text": "final answer"}
            ]}
        ]
    });

    // Without opt-ins nothing reasoning-shaped reaches Codex.
    let projected =
        project_response(&response, &Default::default(), false, false).expect("projection");
    assert_eq!(projected["output"].as_array().map(Vec::len), Some(1));
    assert_eq!(projected["output"][0]["content"][0]["text"], "final answer");

    // Opt-ins keep only the opaque/summary forms, never plaintext chains.
    let projected = project_response(&response, &Default::default(), true, true).expect("kept");
    assert_eq!(projected["output"][0]["type"], "reasoning");
    assert_eq!(
        projected["output"][0]["summary"][0]["text"],
        json!("thinking hard")
    );
    assert_eq!(projected["output"][0]["encrypted_content"], "gAAAA");
}

#[test]
fn stream_projector_suppresses_reasoning_and_relays_answer_deltas() {
    let mut projector = PortableStreamProjector::new(Default::default(), false, false);
    let events = [
        json!({"type": "response.reasoning_summary_part.added", "item_id": "r1", "part": {"type": "summary_text"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "r1"}}),
        json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "hello"}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "id": "m1",
            "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}}),
    ];
    let projected: Vec<Value> = events
        .iter()
        .filter_map(|event| projector.project(event).expect("stream event"))
        .collect();
    let kinds: Vec<&str> = projected
        .iter()
        .map(|event| event["type"].as_str().expect("event type"))
        .collect();
    assert_eq!(
        kinds,
        vec!["response.output_text.delta", "response.output_item.done"]
    );
    assert!(projected.iter().all(|event| {
        !event.to_string().contains("reasoning") && !event.to_string().contains("summary_text")
    }));
}

#[test]
fn failed_upstream_responses_require_an_error_and_map_to_stream_error() {
    let error = validate_responses_body(&json!({"status": "failed", "output": []}), false)
        .expect_err("failed without error");
    assert!(error.to_string().contains("failed status without an error"));

    let observation = terminal_observation(
        &json!({
            "status": "failed",
            "error": {"code": "upstream_down"},
            "output": []
        }),
        false,
    )
    .expect("terminal observation");
    assert_eq!(observation.status, 502);
    assert!(!observation.success);
    assert_eq!(observation.error_class, "stream_error");
}
