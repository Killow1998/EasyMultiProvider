use emp_protocol::portable_responses::{custom_tool_names, project_request, project_response};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn request_fixtures() -> Value {
    json!([
        {
            "provider": {
                "protocol": "responses",
                "auth_mode": "api_key",
                "base_url": "https://gateway.example/v1"
            },
            "body": {
                "model": "provider/model",
                "instructions": "existing instruction",
                "client_metadata": {"thread_id": "private"},
                "input": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "visible"},
                            {"type": "input_image", "image_url": {"url": "data:IMAGE/PNG;base64,AA=="}, "detail": "original"}
                        ]
                    },
                    {"type": "message", "role": "system", "content": "system instruction"},
                    {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "developer instruction"}]},
                    {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "private chain"}]},
                    {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"EMP\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": [{"type": "output_text", "text": "result"}]}
                ],
                "tools": [{"type": "function", "name": "search", "description": "Search", "parameters": {"type": "object"}, "strict": true}],
                "stream": true
            },
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {
                "model": "provider/model",
                "input": [
                    {"type": "additional_tools", "tools": [{"type": "namespace", "name": "functions", "tools": [{"type": "custom", "name": "exec", "description": "Run code"}]}]},
                    {"type": "custom_tool_call", "id": "ctc_original", "call_id": "call_custom", "name": "exec", "input": "text('ok')"},
                    {"type": "custom_tool_call_output", "call_id": "call_custom", "output": "ok"},
                    {"type": "reasoning", "id": "rs_state", "status": "completed", "encrypted_content": "opaque", "content": [{"type": "reasoning_text", "text": "private"}]},
                    {"type": "function_call_output", "name": "diagnostic", "namespace": "tools", "output": "standalone"}
                ],
                "tool_choice": {"type": "custom", "name": "exec"},
                "stream": null
            },
            "preserve_state": true
        },
        {
            "provider": {
                "protocol": "responses",
                "auth_mode": "api_key",
                "base_url": "https://api.openai.com:443/v1/responses/"
            },
            "body": {
                "model": "gpt-test",
                "input": [{"type": "compaction", "encrypted_content": "emp1:U3VtbWFyeSB0ZXh0Lg=="}],
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "thread-fixture",
                "unknown": "drop"
            },
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": null, "instructions": null},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": "continue", "previous_response_id": "resp_private"},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": "hello", "stream": "false"},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [{"type": "agent_message", "content": [{"type": "input_text", "text": "Payload:"}, {"type": "encrypted_content", "encrypted_content": "private"}]}]},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [{"type": "compaction", "encrypted_content": "private-state"}]},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [{"type": "compaction", "encrypted_content": "emp1:%%%%"}]},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [{"type": "item_reference", "id": "item_private"}]},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [{"type": "unsupported_item", "content": [{"type": "custom_part", "text": "private"}]}]},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [{"type": "function_call_output", "call_id": "unbound", "output": "private"}]},
            "preserve_state": false
        },
        {
            "provider": {"protocol": "responses", "auth_mode": "api_key"},
            "body": {"input": [], "tools": [
                {"type": "namespace", "name": "first", "tools": [{"type": "function", "name": "same", "parameters": {"type": "object"}}]},
                {"type": "namespace", "name": "second", "tools": [{"type": "function", "name": "same", "parameters": {"type": "object"}}]}
            ]},
            "preserve_state": false
        }
    ])
}

fn response_fixtures() -> Value {
    json!([
        {
            "body": {"tools": []},
            "response": {
                "id": "resp_fixture",
                "status": "completed",
                "reasoning_content": "top private",
                "output": [
                    {"id": "rs_fixture", "type": "reasoning", "content": [{"type": "reasoning_text", "text": "private"}]},
                    {"id": "msg_fixture", "type": "message", "role": "assistant", "reasoning": "private", "content": [
                        {"type": "output_text", "text": "answer"},
                        {"type": "THINKING_TEXT", "text": "private"},
                        {"type": "output_image", "image_url": "data:IMAGE/PNG;base64,AA=="}
                    ]}
                ]
            },
            "preserve_summary": false,
            "preserve_state": false
        },
        {
            "body": {"tools": [{"type": "custom", "name": "exec", "description": "Run code"}]},
            "response": {
                "status": "completed",
                "output": [
                    {"id": "rs_fixture", "type": "reasoning", "status": "completed", "encrypted_content": "opaque", "summary": [
                        {"type": "summary_text", "text": "bounded summary"},
                        {"type": "reasoning_text", "text": "private"}
                    ], "content": [{"type": "reasoning_text", "text": "private"}]},
                    {"id": "call_fixture", "type": "function_call", "call_id": "call_fixture", "name": "exec", "arguments": "{\"input\":\"text('ok')\"}"}
                ]
            },
            "preserve_summary": true,
            "preserve_state": true
        },
        {
            "body": {},
            "response": {"status": "completed", "output": null},
            "preserve_summary": false,
            "preserve_state": false
        },
        {
            "body": {},
            "response": {"status": "completed", "output": "invalid"},
            "preserve_summary": false,
            "preserve_state": false
        },
        {
            "body": {},
            "response": {"status": "completed", "output": ["invalid"]},
            "preserve_summary": false,
            "preserve_state": false
        },
        {
            "body": {},
            "response": {"status": "completed", "output": [{"type": "compaction", "encrypted_content": "private"}]},
            "preserve_summary": false,
            "preserve_state": false
        }
    ])
}

fn python_oracle(fixtures: &Value, mode: &str) -> Option<Value> {
    let python = std::env::var("EMP_PYTHON_INTEROP").ok()?;
    let script = r#"
import json, sys
from easy_multi_provider.dialects import ProjectionError, custom_tool_names, project_request, project_response

payload = json.load(sys.stdin)
results = []
for case in payload["cases"]:
    try:
        if payload["mode"] == "request":
            value = project_request(case["provider"], case["body"], preserve_reasoning_state=case["preserve_state"])
        else:
            value = project_response(
                {"protocol": "responses", "auth_mode": "api_key"},
                case["response"],
                custom_names=custom_tool_names(case["body"]),
                preserve_reasoning_summary=case["preserve_summary"],
                preserve_reasoning_state=case["preserve_state"],
            )
        results.append({"ok": True, "value": value})
    except ProjectionError as exc:
        results.append({
            "ok": False,
            "error": {
                "index": exc.index,
                "item_type": exc.item_type,
                "part_types": list(exc.part_types),
                "failure_class": exc.failure_class,
            },
        })
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
        .expect("spawn Python portable Responses oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&json!({"mode": mode, "cases": fixtures}))
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write portable Responses fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python portable Responses oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(serde_json::from_slice(&output.stdout).expect("Python oracle JSON"))
}

fn error_value(error: &emp_protocol::portable_responses::PortableProjectionError) -> Value {
    json!({
        "ok": false,
        "error": {
            "index": error.index(),
            "item_type": error.item_type(),
            "part_types": error.part_types(),
            "failure_class": error.failure_class(),
        }
    })
}

#[test]
fn portable_request_projection_matches_live_python_oracle_when_configured() {
    let fixtures = request_fixtures();
    let Some(oracle) = python_oracle(&fixtures, "request") else {
        return;
    };
    let rust = fixtures
        .as_array()
        .expect("request cases")
        .iter()
        .map(|case| {
            let provider = case["provider"].as_object().expect("provider object");
            match project_request(
                provider,
                &case["body"],
                case["preserve_state"].as_bool().expect("preserve state"),
            ) {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(error) => error_value(&error),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}

#[test]
fn portable_response_projection_matches_live_python_oracle_when_configured() {
    let fixtures = response_fixtures();
    let Some(oracle) = python_oracle(&fixtures, "response") else {
        return;
    };
    let rust = fixtures
        .as_array()
        .expect("response cases")
        .iter()
        .map(|case| {
            let names = custom_tool_names(&case["body"]).expect("custom tool names");
            match project_response(
                &case["response"],
                &names,
                case["preserve_summary"]
                    .as_bool()
                    .expect("preserve summary"),
                case["preserve_state"].as_bool().expect("preserve state"),
            ) {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(error) => error_value(&error),
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}

#[test]
fn portable_projection_has_a_non_oracle_smoke_contract() {
    let provider = Map::from_iter([
        ("protocol".to_owned(), Value::String("responses".to_owned())),
        ("auth_mode".to_owned(), Value::String("api_key".to_owned())),
    ]);
    let body = json!({
        "model": "model",
        "input": [{"type": "message", "role": "user", "content": "hello"}],
    });
    let projected = project_request(&provider, &body, false).expect("portable request");
    assert_eq!(projected["stream"], false);
    assert_eq!(projected["input"][0]["content"], "hello");
}
