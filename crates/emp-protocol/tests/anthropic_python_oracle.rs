use emp_protocol::anthropic_projection::{
    AnthropicIds, AnthropicStream, response_from_anthropic, responses_to_anthropic,
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn normalize_generated_ids(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(normalize_generated_ids),
        Value::Object(object) => {
            for key in ["id", "item_id"] {
                if let Some(Value::String(id)) = object.get_mut(key) {
                    if id.starts_with("resp_") {
                        *id = "resp_normalized".to_owned();
                    } else if id.starts_with("msg_") {
                        *id = "msg_normalized".to_owned();
                    }
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

#[test]
fn stream_projection_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = json!([
        {
            "model": "external/model",
            "custom_names": ["render"],
            "body": {
                "model": "external/model", "input": "Use tools", "stream": true,
                "tools": [
                    {"type": "function", "name": "lookup", "parameters": {}},
                    {"type": "custom", "name": "render"}
                ]
            },
            "events": [
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "before"}},
                {"type": "content_block_stop", "index": 0},
                {"type": "content_block_start", "index": 1, "content_block": {"type": "thinking", "thinking": "private"}},
                {"type": "content_block_delta", "index": 1, "delta": {"type": "thinking_delta", "thinking": "private"}},
                {"type": "content_block_stop", "index": 1},
                {"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "call_lookup", "name": "lookup", "input": {}}},
                {"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}},
                {"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"EMP\"}"}},
                {"type": "content_block_stop", "index": 2},
                {"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "call_render", "name": "render", "input": {}}},
                {"type": "content_block_delta", "index": 3, "delta": {"type": "input_json_delta", "partial_json": "{\"input\":\"draw\"}"}},
                {"type": "content_block_stop", "index": 3},
                {"type": "content_block_start", "index": 4, "content_block": {"type": "text", "text": "after"}},
                {"type": "content_block_stop", "index": 4},
                {"type": "message_delta", "delta": {"stop_reason": "tool_use"}},
                {"type": "message_stop"}
            ]
        },
        {
            "model": "external/model",
            "custom_names": [],
            "body": {"model": "external/model", "input": "Hello", "stream": true},
            "events": [
                {"type": "message_start", "message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 3, "output_tokens": 1}}},
                {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "partial"}},
                {"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 7}},
                {"type": "message_stop"}
            ]
        }
    ]);
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider import router
from easy_multi_provider.transport import sse_json_events

class Upstream:
    def __init__(self, events):
        self.events = events
    def __iter__(self):
        wire = "".join("data: " + json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n\n" for event in self.events)
        return iter(wire.encode("utf-8").splitlines(keepends=True))
    def close(self):
        pass

def normalize(value):
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        result = {key: normalize(item) for key, item in value.items()}
        for key in ("id", "item_id"):
            item_id = result.get(key)
            if isinstance(item_id, str) and item_id.startswith("resp_"):
                result[key] = "resp_normalized"
            elif isinstance(item_id, str) and item_id.startswith("msg_"):
                result[key] = "msg_normalized"
        return result
    return value

result = []
for case in json.load(sys.stdin):
    provider = {"id": "fixture", "protocol": "anthropic_messages", "auth_mode": "anthropic_api_key", "base_url": "https://example.invalid/v1"}
    with patch.object(router, "_request", return_value=Upstream(case["events"])):
        result.append(normalize(list(sse_json_events(router.stream_anthropic_completion(provider, case["body"], {"id": case["model"]}, {})))))
json.dump(result, sys.stdout, ensure_ascii=False, separators=(",", ":"))
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
                let mut stream = AnthropicStream::new(
                    case["model"].as_str().unwrap(),
                    AnthropicIds::new("resp_rust", "msg_rust"),
                    &names,
                );
                let mut events = vec![stream.start_event().unwrap().value];
                for event in case["events"].as_array().unwrap() {
                    events.extend(
                        stream
                            .push(event)
                            .unwrap()
                            .into_iter()
                            .map(|event| event.value),
                    );
                }
                events.extend(
                    stream
                        .finish()
                        .unwrap()
                        .into_iter()
                        .map(|event| event.value),
                );
                Value::Array(events)
            })
            .collect(),
    );
    normalize_generated_ids(&mut actual);
    assert_eq!(actual, expected);
}

#[test]
fn stream_failures_match_live_python_status_and_class_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = json!([
        [
            {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "partial"}},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}
        ],
        [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "call_bad", "name": "lookup", "input": {}}},
            {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}},
            {"type": "content_block_stop", "index": 0}
        ],
        [
            {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "answer"}},
            {"type": "message_delta", "delta": {"stop_reason": "pause_turn"}},
            {"type": "message_stop"}
        ],
        [
            {"type": "content_block_start", "index": 0, "content_block": {"type": "redacted_thinking", "data": "private"}},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}},
            {"type": "message_stop"}
        ]
    ]);
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider import router
from easy_multi_provider.transport import sse_json_events

class Upstream:
    def __init__(self, events):
        self.events = events
    def __iter__(self):
        wire = "".join("data: " + json.dumps(event, separators=(",", ":")) + "\n\n" for event in self.events)
        return iter(wire.encode().splitlines(keepends=True))
    def close(self):
        pass

provider = {"id": "fixture", "protocol": "anthropic_messages", "auth_mode": "anthropic_api_key", "base_url": "https://example.invalid/v1"}
body = {"model": "external/model", "input": "Hello", "stream": True, "tools": [{"type": "function", "name": "lookup", "parameters": {}}]}
result = []
for case in json.load(sys.stdin):
    with patch.object(router, "_request", return_value=Upstream(case)):
        terminal = list(sse_json_events(router.stream_anthropic_completion(provider, body, {"id": "external/model"}, {})))[-1]
    error = terminal["response"]["error"]
    result.append({"status": error["status"], "error_class": error["error_class"]})
json.dump(result, sys.stdout, separators=(",", ":"))
"#;
    let expected = oracle(&python, script, &fixture);
    let actual = Value::Array(
        fixture
            .as_array()
            .unwrap()
            .iter()
            .map(|case| {
                let mut stream = AnthropicStream::new(
                    "external/model",
                    AnthropicIds::new("resp_rust", "msg_rust"),
                    &[],
                );
                stream.start_event().unwrap();
                let mut failure = None;
                for event in case.as_array().unwrap() {
                    if let Err(error) = stream.push(event) {
                        failure = Some(error);
                        break;
                    }
                }
                let error = failure.unwrap_or_else(|| stream.finish().unwrap_err());
                json!({"status": error.status(), "error_class": error.error_class()})
            })
            .collect(),
    );
    assert_eq!(actual, expected);
}
