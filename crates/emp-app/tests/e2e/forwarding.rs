use crate::support::{NATIVE_TOKEN, PROVIDER_KEY, Upstream, Workspace};
use serde_json::{Value, json};

fn output_types(response: &Value) -> Vec<&str> {
    response["output"]
        .as_array()
        .expect("output array")
        .iter()
        .map(|item| item["type"].as_str().expect("item type"))
        .collect()
}

fn joined_deltas(events: &[Value], kind: &str) -> String {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| event["delta"].as_str().expect("delta text"))
        .collect()
}

fn chat_usage() -> Value {
    json!({
        "prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150,
        "prompt_tokens_details": {"cached_tokens": 80},
        "completion_tokens_details": {"reasoning_tokens": 20},
    })
}

fn responses_usage() -> Value {
    json!({
        "input_tokens": 120, "output_tokens": 30, "total_tokens": 150,
        "input_tokens_details": {"cached_tokens": 80},
        "output_tokens_details": {"reasoning_tokens": 20},
    })
}

#[test]
fn codex_sees_the_enabled_model_catalog() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let response = emp.codex("GET", "/v1/models?client_version=0.156.1", None);
    assert_eq!(response.status, 200, "{}", response.text());
    let slugs: Vec<String> = response.json()["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|model| model["slug"].as_str().expect("slug").to_owned())
        .collect();
    for id in [
        "test/model",
        "anthropic/model",
        "responses/model",
        "native/model",
    ] {
        assert!(
            slugs.iter().any(|slug| slug == id),
            "{id} missing from {slugs:?}"
        );
    }
    upstream.assert_idle();
}

#[test]
fn chat_completion_omits_raw_reasoning_and_preserves_answer_and_usage() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_json(&json!({
        "choices": [{"message": {"reasoning_content": "Check the sum.", "content": "Four."},
                     "finish_reason": "stop"}],
        "usage": chat_usage(),
    }));

    let response = emp.responses(&json!({
        "model": "test/model", "input": "hello", "reasoning": {"effort": "low"},
    }));
    assert_eq!(response.status, 200, "{}", response.text());
    let forwarded = upstream.next_request();
    assert_eq!(forwarded.path, "/v1/chat/completions");
    assert_eq!(
        forwarded.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {PROVIDER_KEY}").as_str())
    );
    let forwarded = forwarded.body.expect("chat body");
    assert_eq!(forwarded["model"], "upstream-model");
    upstream.assert_idle();

    let result = response.json();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["output_text"], "Four.");
    assert_eq!(result["output"][0]["phase"], "final_answer");
    assert_eq!(result["usage"], responses_usage());
    assert_eq!(output_types(&result), ["message"]);
    assert!(!response.text().contains("Check the sum."));
}

#[test]
fn chat_stream_omits_raw_reasoning_and_preserves_answer_and_usage() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_chat_stream(&[
        json!({"choices": [{"delta": {"content": null, "reasoning_content": "Check "}}]}),
        json!({"choices": [{"delta": {"reasoning_content": "the sum.", "content": "Four."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": chat_usage()}),
    ]);

    let response = emp.responses(&json!({"model": "test/model", "input": "hello", "stream": true}));
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.contains("text/event-stream"))
    );
    let forwarded = upstream.next_request().body.expect("chat body");
    assert_eq!(forwarded["stream"], true);
    upstream.assert_idle();

    let events = response.sse_events();
    assert_eq!(joined_deltas(&events, "response.reasoning_text.delta"), "");
    assert!(!response.text().contains("Check "));
    assert!(!response.text().contains("the sum."));
    assert_eq!(
        joined_deltas(&events, "response.output_text.delta"),
        "Four."
    );
    let completed = events.last().expect("terminal event");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["output_text"], "Four.");
    assert_eq!(completed["response"]["output"][0]["phase"], "final_answer");
    assert_eq!(output_types(&completed["response"]), ["message"]);
    assert_eq!(completed["response"]["usage"], responses_usage());
}

#[test]
fn chat_tool_preamble_is_commentary_and_tool_result_reaches_final_answer() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    for streaming in [false, true] {
        let tool = json!({"id": "call_read", "type": "function", "function": {
            "name": "read_file", "arguments": "{\"path\":\"example.txt\"}"
        }});
        if streaming {
            upstream.reply_chat_stream(&[
                json!({"choices": [{"delta": {"content": "I will read the file."}}]}),
                json!({"choices": [{"delta": {"tool_calls": [tool]}, "finish_reason": "tool_calls"}]}),
            ]);
        } else {
            upstream.reply_json(&json!({"choices": [{"message": {
                "content": "I will read the file.", "tool_calls": [tool]
            }, "finish_reason": "tool_calls"}]}));
        }
        let first = emp.responses(
            &json!({"model": "test/model", "input": "read the file", "stream": streaming}),
        );
        assert_eq!(first.status, 200, "{}", first.text());
        let projected = if streaming {
            let events = first.sse_events();
            let message = events
                .iter()
                .find(|event| {
                    event["type"] == "response.output_item.done"
                        && event["item"]["type"] == "message"
                })
                .expect("progress message");
            assert_eq!(message["item"]["phase"], "commentary");
            events.last().expect("completion")["response"].clone()
        } else {
            first.json()
        };
        assert_eq!(projected["output"][0]["phase"], "commentary");
        assert_eq!(projected["output"][1]["type"], "function_call");
        upstream.next_request();
        upstream.reply_json(&json!({"choices": [{"message": {"content": "The file says OK."}, "finish_reason": "stop"}]}));
        let mut input = vec![json!({"role": "user", "content": "read the file"})];
        input.extend(
            projected["output"]
                .as_array()
                .expect("items")
                .iter()
                .cloned(),
        );
        input.push(json!({"type": "function_call_output", "call_id": "call_read", "output": "OK"}));
        let final_response = emp.responses(&json!({"model": "test/model", "input": input}));
        assert_eq!(final_response.status, 200, "{}", final_response.text());
        assert_eq!(final_response.json()["output"][0]["phase"], "final_answer");
        let forwarded = upstream.next_request().body.expect("follow-up body");
        assert!(
            forwarded["messages"]
                .as_array()
                .expect("history")
                .iter()
                .any(|message| message["role"] == "tool"
                    && message["tool_call_id"] == "call_read"
                    && message["content"] == "OK")
        );
        upstream.assert_idle();
    }
}

#[test]
fn chat_raw_tool_markup_returns_a_protocol_error_without_leaking_parameters() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let markup = "</｜DSML｜ parameter>private parameter value</｜DSML｜ calls>";
    for streaming in [false, true] {
        if streaming {
            upstream.reply_chat_stream(&[
                json!({"choices": [{"delta": {"content": "I will check the file.\n</｜D"}}]}),
                json!({"choices": [{"delta": {"content": "SML｜ parameter>private parameter value"}, "finish_reason": "stop"}]}),
            ]);
        } else {
            upstream.reply_json(
                &json!({"choices": [{"message": {"content": markup}, "finish_reason": "stop"}]}),
            );
        }
        let response = emp.responses(
            &json!({"model": "test/model", "input": "check the file", "stream": streaming}),
        );
        assert!(!response.text().contains("private parameter value"));
        assert!(!response.text().contains("</｜DSML｜"));
        if streaming {
            assert_eq!(response.status, 200);
            let events = response.sse_events();
            assert_eq!(
                events.last().expect("terminal error")["type"],
                "response.failed"
            );
            assert!(
                !events
                    .iter()
                    .any(|event| event["type"] == "response.completed")
            );
        } else {
            assert_eq!(response.status, 502, "{}", response.text());
        }
        upstream.next_request();
        upstream.assert_idle();
    }
}

#[test]
fn chat_stream_refusal_keeps_the_usage_only_tail() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_chat_stream(&[
        json!({"choices": [{"delta": {"refusal": "I cannot "}}]}),
        json!({"choices": [{"delta": {"refusal": "assist."}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": chat_usage()}),
    ]);

    let response = emp.responses(&json!({"model": "test/model", "input": "hello", "stream": true}));
    assert_eq!(response.status, 200, "{}", response.text());
    upstream.next_request();
    upstream.assert_idle();

    let events = response.sse_events();
    assert_eq!(
        joined_deltas(&events, "response.refusal.delta"),
        "I cannot assist."
    );
    let completed = events.last().expect("terminal event");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["usage"], responses_usage());
    assert_eq!(
        completed["response"]["output"][0]["content"],
        json!([{"type": "refusal", "refusal": "I cannot assist."}])
    );
}

#[test]
fn anthropic_tool_use_becomes_a_paired_function_call() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_json(&json!({
        "id": "upstream-message", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "Calculating."},
                    {"type": "tool_use", "id": "tool_fixture", "name": "calculator",
                     "input": {"x": 4}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 3, "output_tokens": 2},
    }));

    let response = emp.responses(&json!({
        "model": "anthropic/model", "input": "hello",
        "tools": [{"type": "function", "name": "calculator",
                   "parameters": {"type": "object", "properties": {"x": {"type": "number"}}}}],
    }));
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(upstream.next_request().path, "/v1/messages");
    upstream.assert_idle();

    let result = response.json();
    let call = result["output"]
        .as_array()
        .expect("output")
        .last()
        .expect("function call");
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["name"], "calculator");
    assert_eq!(call["call_id"], "tool_fixture");
    let arguments: Value =
        serde_json::from_str(call["arguments"].as_str().expect("arguments")).expect("JSON");
    assert_eq!(arguments, json!({"x": 4}));
}

#[test]
fn native_response_passes_through_unknown_fields_and_model() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let native = json!({
        "id": "upstream-response", "object": "response", "status": "completed",
        "model": "upstream-native-alias", "output": [],
        "future_native_field": {"opaque": ["retained"]},
    });
    upstream.reply_json(&native);

    let response = emp.responses(&json!({"model": "native/model", "input": "hello"}));
    assert_eq!(response.status, 200, "{}", response.text());
    let forwarded = upstream.next_request();
    assert_eq!(forwarded.path, "/v1/responses");
    // Forward-auth routes carry the caller's own Codex login upstream.
    assert_eq!(
        forwarded.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {NATIVE_TOKEN}").as_str())
    );
    upstream.assert_idle();
    assert_eq!(response.json(), native);
}

#[test]
fn upstream_503_is_visible_once_with_normalized_retry_advice() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let error = json!({"error": {"message": "temporarily unavailable", "type": "server_error"}});
    for model in ["test/model", "native/model"] {
        for advice in [
            "Thu, 01 Jan 1970 00:00:30 GMT",
            "Thursday, 01-Jan-70 00:00:30 GMT",
        ] {
            upstream.configure_with_headers(
                503,
                "application/json",
                error.to_string(),
                &[("Retry-After", advice)],
            );
            let response = emp.responses(&json!({"model": model, "input": "hello"}));
            assert_eq!(response.status, 503, "{model}: {}", response.text());
            // A date already in the past means "retry now", never a stale date.
            assert_eq!(
                response.header("retry-after"),
                Some("0"),
                "{model} {advice}"
            );
            let error = response.json()["error"].clone();
            assert_eq!(error["code"], "upstream_unavailable", "{error}");
            assert_eq!(error["retry_after_seconds"], 0, "{error}");
            upstream.next_request();
            upstream.assert_idle();
        }
    }
}
