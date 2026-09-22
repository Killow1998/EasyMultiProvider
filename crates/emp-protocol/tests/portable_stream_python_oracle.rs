use emp_protocol::portable_responses::PortableStreamProjector;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, Stdio};

fn fixtures() -> Value {
    json!([
        {
            "custom_names": [],
            "preserve_summary": false,
            "preserve_state": false,
            "events": [
                {"type": "response.output_item.added", "output_index": 0, "item": {"id": "rs_private", "type": "reasoning", "content": []}},
                {"type": "response.reasoning_text.delta", "item_id": "rs_private", "delta": "private chain"},
                {"type": "response.output_item.added", "output_index": 1, "item": {"id": "msg_visible", "type": "message", "role": "assistant", "content": []}},
                {"type": "response.output_text.delta", "item_id": "msg_visible", "delta": "answer"},
                {"type": "response.content_part.added", "item_id": "msg_visible", "part": {"type": "thinking_text", "text": "private"}},
                {"type": "response.completed", "response": {"status": "completed", "output": [
                    {"id": "rs_private", "type": "reasoning", "content": [{"type": "reasoning_text", "text": "private chain"}]},
                    {"id": "msg_visible", "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer", "annotations": []}]}
                ]}}
            ]
        },
        {
            "custom_names": [],
            "preserve_summary": true,
            "preserve_state": true,
            "events": [
                {"type": "response.output_item.added", "output_index": 0, "item": {
                    "id": "rs_state", "type": "reasoning", "status": "in_progress",
                    "encrypted_content": "opaque", "summary": [{"type": "summary_text", "text": "bounded"}],
                    "content": [{"type": "reasoning_text", "text": "private"}]
                }},
                {"type": "response.reasoning_summary_text.delta", "item_id": "rs_state", "delta": "bounded"},
                {"type": "response.reasoning_text.delta", "item_id": "rs_state", "delta": "private"},
                {"type": "response.completed", "response": {"status": "completed", "output": [{
                    "id": "rs_state", "type": "reasoning", "status": "completed",
                    "encrypted_content": "opaque", "summary": [{"type": "summary_text", "text": "bounded"}],
                    "content": [{"type": "reasoning_text", "text": "private"}]
                }]}}
            ]
        },
        {
            "custom_names": ["exec"],
            "preserve_summary": false,
            "preserve_state": false,
            "events": [
                {"type": "response.output_item.added", "output_index": 0, "item": {
                    "id": "call_raw", "type": "function_call", "call_id": "call_custom", "name": "exec", "arguments": ""
                }},
                {"type": "response.function_call_arguments.delta", "item_id": "call_raw", "delta": "{\"input\":\"read"},
                {"type": "response.function_call_arguments.done", "item_id": "call_raw", "arguments": "{\"input\":\"read file\"}"},
                {"type": "response.output_item.done", "output_index": 0, "item": {
                    "id": "call_raw", "type": "function_call", "call_id": "call_custom", "name": "exec", "arguments": "{\"input\":\"read file\"}"
                }},
                {"type": "response.completed", "response": {"status": "completed", "output": [{
                    "id": "call_raw", "type": "function_call", "call_id": "call_custom", "name": "exec", "arguments": "{\"input\":\"read file\"}"
                }]}}
            ]
        },
        {
            "custom_names": [],
            "preserve_summary": false,
            "preserve_state": false,
            "events": [
                {"type": "response.output_item.added", "output_index": 3, "item": {"type": "compaction", "encrypted_content": "private"}}
            ]
        }
    ])
}

fn python_oracle(fixtures: &Value) -> Value {
    let python = std::env::var("EMP_PYTHON_INTEROP").expect("configured Python oracle");
    let script = r#"
import json, sys
from easy_multi_provider.dialects import ProjectionError, project_stream_event

results = []
for case in json.load(sys.stdin):
    suppressed = set()
    custom_state = {}
    events = []
    failure = None
    for event in case["events"]:
        try:
            value = project_stream_event(
                {"protocol": "responses", "auth_mode": "api_key"}, event,
                suppressed, custom_names=set(case["custom_names"]), custom_state=custom_state,
                preserve_reasoning_summary=case["preserve_summary"],
                preserve_reasoning_state=case["preserve_state"],
            )
            if value is not None:
                events.append(value)
        except ProjectionError as exc:
            failure = {
                "index": exc.index, "item_type": exc.item_type,
                "part_types": list(exc.part_types), "failure_class": exc.failure_class,
            }
            break
    results.append({"events": events, "error": failure})
json.dump(results, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python portable stream oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(fixtures)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write portable stream fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python portable stream oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python oracle JSON")
}

#[test]
fn portable_stream_events_match_live_python_oracle_when_configured() {
    let fixtures = fixtures();
    let Ok(_) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let oracle = python_oracle(&fixtures);
    let rust = fixtures
        .as_array()
        .expect("fixture cases")
        .iter()
        .map(|case| {
            let names = case["custom_names"]
                .as_array()
                .expect("custom names")
                .iter()
                .map(|name| name.as_str().expect("custom name").to_owned())
                .collect::<BTreeSet<_>>();
            let mut projector = PortableStreamProjector::new(
                names,
                case["preserve_summary"].as_bool().expect("summary flag"),
                case["preserve_state"].as_bool().expect("state flag"),
            );
            let mut events = Vec::new();
            let mut failure = Value::Null;
            for event in case["events"].as_array().expect("events") {
                match projector.project(event) {
                    Ok(Some(event)) => events.push(event),
                    Ok(None) => {}
                    Err(error) => {
                        failure = json!({
                            "index": error.index(),
                            "item_type": error.item_type(),
                            "part_types": error.part_types(),
                            "failure_class": error.failure_class(),
                        });
                        break;
                    }
                }
            }
            json!({"events": events, "error": failure})
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}
