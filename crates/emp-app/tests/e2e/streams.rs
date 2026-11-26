use emp_test_support::{Deadline, test_timeout};
use std::io;
use std::time::Duration;

use crate::support::{NATIVE_TOKEN, SseClient, Upstream, WebSocket, WebSocketUpstream, Workspace};
use serde_json::{Value, json};

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn responses_events(id: &str, text: &str) -> (Vec<Value>, Vec<Value>) {
    let message = json!({
        "id": "msg_stream", "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    });
    (
        vec![
            json!({"type": "response.created", "response": {
                "id": id, "object": "response", "status": "in_progress",
                "model": "upstream-model", "output": [],
            }}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "id": "msg_stream", "type": "message", "role": "assistant",
                "status": "in_progress", "content": [],
            }}),
            json!({"type": "response.content_part.added", "item_id": "msg_stream",
                "output_index": 0, "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_stream",
                "output_index": 0, "content_index": 0, "delta": text}),
        ],
        vec![
            json!({"type": "response.output_text.done", "item_id": "msg_stream",
                "output_index": 0, "content_index": 0, "text": text}),
            json!({"type": "response.content_part.done", "item_id": "msg_stream",
                "output_index": 0, "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": []}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": message}),
            json!({"type": "response.completed", "response": {
                "id": id, "object": "response", "status": "completed",
                "model": "upstream-model", "output": [message],
                "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5},
            }}),
        ],
    )
}

fn stream_parts(model: &str) -> (String, String, &'static str) {
    if model == "test/model" {
        (
            sse(&[json!({"id": "chat_stream", "choices": [{
                "index": 0, "delta": {"content": "First visible answer"}, "finish_reason": null,
            }]})]),
            sse(&[json!({"id": "chat_stream", "choices": [{
                "index": 0, "delta": {}, "finish_reason": "stop",
            }], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}})])
                + "data: [DONE]\n\n",
            "/v1/chat/completions",
        )
    } else {
        let (first, tail) = responses_events("resp_stream", "First visible answer");
        (sse(&first), sse(&tail), "/v1/responses")
    }
}

fn next_kind(client: &mut SseClient, kind: &str) -> Value {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    loop {
        let event = client
            .next_event(&deadline)
            .expect("incremental SSE event before deadline")
            .expect("SSE ended before expected event");
        if event["type"] == kind {
            return event;
        }
        assert!(
            !matches!(
                event["type"].as_str(),
                Some("response.completed" | "response.failed" | "response.incomplete" | "error")
            ),
            "terminal event before {kind}: {event}"
        );
    }
}

#[test]
fn sse_visible_delta_arrives_before_upstream_is_released() {
    for model in ["native/model", "responses/model", "test/model"] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        let emp = workspace.start_without_path_cli();
        let (first, tail, path) = stream_parts(model);
        let gate = upstream.gate_stream(first, Some(tail));
        let mut client = SseClient::responses(
            emp.port,
            &json!({
                "model": model, "input": "hello", "stream": true,
            }),
        );
        let request = upstream.next_request();
        assert_eq!(request.path, path);
        assert_eq!(request.body.expect("request JSON")["stream"], true);
        let visible = next_kind(&mut client, "response.output_text.delta");
        assert_eq!(visible["delta"], "First visible answer", "{model}");
        gate.finish();
        let completed = next_kind(&mut client, "response.completed");
        assert_eq!(completed["response"]["status"], "completed");
        assert_eq!(completed["response"]["usage"]["total_tokens"], 5);
        client.stop_at_chunk_boundary().unwrap();
        upstream.assert_idle();
    }
}

#[test]
fn downstream_sse_disconnect_closes_the_still_gated_upstream() {
    for model in ["native/model", "responses/model", "test/model"] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        let emp = workspace.start_without_path_cli();
        let (first, tail, path) = stream_parts(model);
        let gate = upstream.gate_stream(first, Some(tail));
        let mut client = SseClient::responses(
            emp.port,
            &json!({
                "model": model, "input": "hello", "stream": true,
            }),
        );
        assert_eq!(upstream.next_request().path, path);
        assert_eq!(
            next_kind(&mut client, "response.output_text.delta")["delta"],
            "First visible answer"
        );
        client.stop_at_chunk_boundary().unwrap();
        let closed = gate.observe_close(Deadline::after(test_timeout(Duration::from_secs(5))));
        drop(client);
        closed.wait().unwrap();
        upstream.assert_idle();
    }
}

#[cfg(target_os = "linux")]
#[test]
fn downstream_disconnect_closes_external_sse_with_a_blocking_registry_override() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let registry = workspace.root.join("official-models.json");
    let mut command = workspace.command();
    command
        .env("EMP_OFFICIAL_MODELS_PATH", &registry)
        .env("PATH", workspace.root.join("empty-bin"))
        .args(workspace.serve_arguments());
    let emp = crate::support::Emp::spawn(command);
    let fifo = CString::new(registry.as_os_str().as_bytes()).unwrap();
    // SAFETY: fifo is a NUL-terminated path in the fixture's private directory.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    let (first, tail, path) = stream_parts("test/model");
    let gate = upstream.gate_stream(first, Some(tail));
    let mut client = SseClient::responses(
        emp.port,
        &json!({"model":"test/model","input":"hello","stream":true}),
    );
    assert_eq!(upstream.next_forwarded().path, path);
    assert_eq!(
        next_kind(&mut client, "response.output_text.delta")["delta"],
        "First visible answer"
    );
    client.stop_at_chunk_boundary().unwrap();
    let closed = gate.observe_close(Deadline::after(test_timeout(Duration::from_secs(5))));
    drop(client);
    closed.wait().unwrap();
}

#[test]
fn truncated_sse_never_fabricates_a_successful_completion() {
    for model in ["native/model", "responses/model", "test/model"] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        let emp = workspace.start_without_path_cli();
        let (first, _, path) = stream_parts(model);
        let gate = upstream.gate_stream(first, None);
        let mut client = SseClient::responses(
            emp.port,
            &json!({
                "model": model, "input": "hello", "stream": true,
            }),
        );
        assert_eq!(upstream.next_request().path, path);
        assert_eq!(
            next_kind(&mut client, "response.output_text.delta")["delta"],
            "First visible answer"
        );
        gate.finish();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        loop {
            match client.next_event(&deadline) {
                Ok(Some(event)) => {
                    assert_ne!(event["type"], "response.completed", "{model}: {event}");
                }
                Ok(None) => {
                    client
                        .expect_eof(&deadline)
                        .unwrap_or_else(|error| panic!("{model}: truncated stream EOF: {error}"));
                    break;
                }
                Err(error) => {
                    assert!(
                        matches!(
                            error.kind(),
                            io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                        ),
                        "{model}: truncated stream must fail or close promptly: {error}"
                    );
                    break;
                }
            }
        }
        upstream.assert_idle();
    }
}

#[test]
fn responses_websocket_reuses_upstream_across_turns_and_closes_it_on_client_close() {
    let replies = [
        ("resp_ws_1", "First WS answer", true),
        ("resp_ws_2", "Second WS answer", true),
        ("resp_ws_3", "Pending WS answer", false),
    ]
    .into_iter()
    .map(|(id, text, complete)| {
        let (mut first, tail) = responses_events(id, text);
        if complete {
            first.extend(tail);
        }
        first
    })
    .collect();
    let upstream = WebSocketUpstream::start(replies);
    let workspace = Workspace::new();
    workspace.write_config(json!({
        "codex_base_url": upstream.base_url(),
        "providers": [{"id": "native", "base_url": upstream.base_url(),
            "protocol": "responses", "auth_mode": "forward"}],
        "models": [{"id": "native/model", "provider": "native",
            "upstream_id": "upstream-model", "enabled": true}],
    }));
    let emp = workspace.start_without_path_cli();
    let mut client = WebSocket::connect(emp.port);
    for (index, (previous, text)) in [
        (None, "First WS answer"),
        (Some("resp_ws_1"), "Second WS answer"),
        (Some("resp_ws_2"), "Pending WS answer"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut body = json!({"type": "response.create", "model": "native/model",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": text}]}]});
        if let Some(previous) = previous {
            body["previous_response_id"] = json!(previous);
        }
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        let observation = upstream.begin(deadline, "upstream WS turn");
        client.send_json(&body, &deadline);
        let observed = observation.wait().unwrap().expect("upstream WS turn");
        if index == 0 {
            let handshake = observed.handshake.expect("upstream WS handshake");
            assert_eq!(handshake.path, "/v1/responses");
            assert_eq!(
                handshake.headers["authorization"],
                format!("Bearer {NATIVE_TOKEN}")
            );
        }
        let turn = observed.request;
        assert_eq!(turn["type"], "response.create");
        assert_eq!(turn["model"], "upstream-model");
        assert_eq!(
            turn.get("previous_response_id"),
            previous.map(|id| json!(id)).as_ref()
        );
        assert!(turn["input"].to_string().contains(text), "{turn}");
        let mut answer = String::new();
        loop {
            let event = client
                .next_json(&deadline)
                .expect("WS event")
                .expect("WS closed during turn");
            assert!(
                !matches!(
                    event["type"].as_str(),
                    Some("error" | "response.failed" | "response.incomplete")
                ),
                "{event}"
            );
            if event["type"] == "response.output_text.delta" {
                answer.push_str(event["delta"].as_str().expect("WS text delta"));
                if index == 2 {
                    assert_eq!(answer, text);
                    break;
                }
            }
            if event["type"] == "response.completed" {
                assert_eq!(answer, text);
                assert_eq!(event["response"]["id"], format!("resp_ws_{}", index + 1));
                assert_eq!(event["response"]["usage"]["total_tokens"], 5);
                break;
            }
        }
    }
    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let closed = upstream.begin(deadline, "downstream close must close upstream WS");
    client.close(&deadline);
    assert!(closed.wait().unwrap().is_none());
}
