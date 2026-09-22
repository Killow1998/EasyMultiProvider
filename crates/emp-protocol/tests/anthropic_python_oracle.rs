use emp_protocol::anthropic_projection::{
    AnthropicIds, response_from_anthropic, responses_to_anthropic,
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn normalize_generated_ids(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize_generated_ids),
        Value::Object(object) => {
            if let Some(Value::String(id)) = object.get_mut("id") {
                if id.starts_with("resp_") {
                    *id = "resp_normalized".to_owned();
                } else if id.starts_with("msg_") {
                    *id = "msg_normalized".to_owned();
                }
            }
            object.values_mut().for_each(normalize_generated_ids);
        }
        _ => {}
    }
}

fn oracle(python: &str, script: &str, fixture: &Value) -> Value {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python Anthropic oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(serde_json::to_string(fixture).unwrap().as_bytes())
        .expect("write oracle fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python Anthropic oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python oracle JSON")
}

#[test]
fn request_projection_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = json!([
        {
            "model": "claude-controls",
            "body": {
                "instructions": [{"type": "input_text", "text": "Be concise."}],
                "input": "Hello",
                "stream": true,
                "max_output_tokens": 0,
                "temperature": 0.2,
                "top_p": 0.9,
                "stop": "END",
                "reasoning": {"effort": "low"},
                "text": {"format": {"type": "json_schema", "name": "answer", "strict": true, "schema": {"type": "object"}}}
            }
        },
        {
            "model": "claude-tools",
            "body": {
                "input": [
                    {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "Look"},
                        {"type": "input_image", "image_url": "data:image/PNG;BASE64,AA=="},
                        {"type": "input_image", "image_url": "HTTP://example.test/image.png"}
                    ]},
                    {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": {"q": "EMP"}},
                    {"type": "function_call_output", "call_id": "call_1", "output": "result"},
                    {"type": "custom_tool_call", "call_id": "call_2", "name": "shell", "input": ["pwd"]},
                    {"type": "custom_tool_call_output", "call_id": "call_2", "output": "done"}
                ],
                "tools": [
                    {"type": "namespace", "name": "local", "tools": [
                        {"type": "function", "name": "lookup", "description": true, "parameters": {"type": "object"}},
                        {"type": "function", "name": "lookup", "description": true, "parameters": {"type": "object"}}
                    ]},
                    {"type": "custom", "name": "shell"}
                ],
                "tool_choice": {"type": "custom", "name": "shell"},
                "parallel_tool_calls": false
            }
        },
        {
            "model": "claude-history",
            "body": {"input": [
                {"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [{"type": "input_text", "text": "Do work."}]},
                {"type": "function_call_output", "name": "diagnostic", "namespace": "tools", "output": "visible"},
                {"type": "compaction", "encrypted_content": "emp1:U3VtbWFyeSB0ZXh0Lg=="}
            ]}
        },
        {
            "model": "claude-extra-tools",
            "body": {"input": {"type": "additional_tools", "tools": [{"type": "custom", "name": "render"}]}}
        }
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.protocol_projection import responses_to_anthropic
cases = json.load(sys.stdin)
json.dump([responses_to_anthropic(case["body"], case["model"]) for case in cases], sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let expected = oracle(&python, script, &fixture);
    let actual = Value::Array(
        fixture
            .as_array()
            .unwrap()
            .iter()
            .map(|case| {
                responses_to_anthropic(&case["body"], case["model"].as_str().unwrap())
                    .expect("Rust request projection")
            })
            .collect(),
    );
    assert_eq!(actual, expected);
}

#[test]
fn complete_projection_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = json!([
        {
            "model": "claude-complete",
            "custom_names": ["shell"],
            "response": {
                "content": [
                    {"type": "text", "text": "Partial "},
                    {"type": "tool_use", "id": "call_1", "name": "lookup", "input": {"q": "EMP"}},
                    {"type": "tool_use", "id": "call_2", "name": "shell", "input": {"input": "pwd"}},
                    {"type": "text", "text": "answer"}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 100, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 3, "output_tokens": 7, "cache_creation": {"ephemeral_1h_input_tokens": 2}}
            }
        },
        {
            "model": "claude-limited",
            "custom_names": [],
            "response": {"content": [{"type": "text", "text": "partial"}], "stop_reason": "max_tokens", "usage": {"output_tokens": 2}}
        }
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.protocol_projection import _response_from_anthropic
def normalize(value):
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        result = {key: normalize(item) for key, item in value.items()}
        item_id = result.get("id")
        if isinstance(item_id, str) and item_id.startswith("resp_"):
            result["id"] = "resp_normalized"
        elif isinstance(item_id, str) and item_id.startswith("msg_"):
            result["id"] = "msg_normalized"
        return result
    return value
cases = json.load(sys.stdin)
json.dump([normalize(_response_from_anthropic(case["response"], case["model"], set(case["custom_names"]))) for case in cases], sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let expected = oracle(&python, script, &fixture);
    let mut actual = Value::Array(
        fixture
            .as_array()
            .unwrap()
            .iter()
            .map(|case| {
                let names = case["custom_names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|name| name.as_str().unwrap())
                    .collect::<Vec<_>>();
                let mut ids = AnthropicIds::new("resp_rust", "msg_rust");
                response_from_anthropic(
                    &case["response"],
                    case["model"].as_str().unwrap(),
                    &names,
                    &mut ids,
                )
                .expect("Rust response projection")
            })
            .collect(),
    );
    normalize_generated_ids(&mut actual);
    assert_eq!(actual, expected);
}
