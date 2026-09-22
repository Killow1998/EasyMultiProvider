use emp_protocol::{ChatFrame, ChatIds, ChatStream, response_from_chat, responses_to_chat};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixtures() -> Value {
    json!([
        {
            "model": "test/model",
            "custom_names": [],
            "response": {
                "choices": [{
                    "message": {
                        "reasoning_content": "Check the sum.",
                        "reasoning": "lower precedence",
                        "content": "Four.",
                        "refusal": "Boundary note."
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
            }
        },
        {
            "model": "tool/model",
            "custom_names": ["shell"],
            "response": {
                "choices": [{
                    "message": {"tool_calls": [{
                        "id": "call_custom",
                        "function": {"name": "shell", "arguments": "{\"input\":\"pwd\"}"},
                        "extra_content": {"provider": "value"}
                    }]},
                    "finish_reason": "tool_calls"
                }]
            }
        },
        {
            "model": "limited/model",
            "custom_names": [],
            "response": {
                "choices": [{
                    "message": {"content": [{"type": "text", "text": "partial"}]},
                    "finish_reason": "length"
                }],
                "service_tier": "priority"
            }
        }
    ])
}

fn normalize_generated_ids(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                normalize_generated_ids(value);
            }
        }
        Value::Object(object) => {
            for key in ["id", "item_id"] {
                if let Some(Value::String(id)) = object.get_mut(key) {
                    if id.starts_with("resp_") {
                        *id = "resp_normalized".to_owned();
                    } else if id.starts_with("msg_") {
                        *id = "msg_normalized".to_owned();
                    } else if id.starts_with("rs_") {
                        *id = "rs_normalized".to_owned();
                    }
                }
            }
            for value in object.values_mut() {
                normalize_generated_ids(value);
            }
        }
        _ => {}
    }
}

fn stream_fixtures() -> Value {
    json!([
        [
            {"choices": [{"delta": {"reasoning_content": "Check "}}]},
            {"choices": [{"delta": {"reasoning_content": "the sum.", "content": "Four."}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
            {"choices": [], "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}}
        ],
        [
            {"choices": [{"delta": {"content": "First."}}]},
            {"choices": [{"delta": {"reasoning_text": "Late reasoning."}, "finish_reason": "stop"}]}
        ],
        [
            {"choices": [{"delta": {"content": "Calculating."}}]},
            {"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_fixture", "type": "function",
                "function": {"name": "calculator", "arguments": "{\"x\":4}"},
                "extra_content": {"vendor": true}
            }]}, "finish_reason": "tool_calls"}]}
        ],
        [
            {"choices": [{"delta": {"refusal": "I cannot "}}]},
            {"choices": [{"delta": {"refusal": "assist."}, "finish_reason": "stop"}]},
            {"choices": [], "usage": {"completion_tokens": 2}}
        ]
    ])
}

fn request_fixtures() -> Value {
    json!([
        {
            "model": "upstream-chat",
            "body": {
                "instructions": "Be concise.",
                "input": "Hello",
                "stream": true,
                "temperature": 0.2,
                "top_p": 0.9,
                "stop": ["END"],
                "max_output_tokens": 42,
                "reasoning": {"effort": "low"}
            }
        },
        {
            "model": "vision-chat",
            "body": {
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "Describe "},
                        {"type": "input_image", "image_url": {"url": "data:image/png;base64,AA=="}, "detail": "original"},
                        {"type": "refusal", "refusal": "boundary"}
                    ]
                }],
                "text": {"format": {
                    "type": "json_schema",
                    "name": "answer_schema",
                    "strict": true,
                    "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}
                }}
            }
        },
        {
            "model": "tool-chat",
            "body": {
                "input": [
                    {"type": "function_call", "call_id": "call_1", "name": "search", "arguments": "{\"q\":\"EMP\"}", "extra_content": {"vendor": true}},
                    {"type": "reasoning", "encrypted_content": "opaque-not-forwarded"},
                    {"type": "function_call_output", "call_id": "call_1", "output": [{"type": "output_text", "text": "result"}]},
                    {"type": "agent_message", "author": "/root", "recipient": "/root/worker", "content": [
                        {"type": "input_text", "text": "Message Type: NEW_TASK"},
                        {"type": "input_text", "text": "Implement it."}
                    ]},
                    {"type": "function_call_output", "name": "diagnostic", "namespace": "tools", "output": "standalone"},
                    {"type": "additional_tools", "tools": [{"type": "custom", "name": "shell", "description": "Run code"}]}
                ],
                "tools": [{"type": "function", "name": "search", "description": "Search", "parameters": {"type": "object"}}],
                "tool_choice": {"type": "custom", "name": "shell"},
                "parallel_tool_calls": false
            }
        },
        {
            "model": "history-chat",
            "body": {
                "input": [{"type": "compaction", "encrypted_content": "emp1:U3VtbWFyeSB0ZXh0Lg=="}]
            }
        }
    ])
}

#[test]
fn complete_chat_projection_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = fixtures();
    let script = r#"
import json, sys
from easy_multi_provider.protocol_projection import _response_from_chat

def normalize(value):
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        result = {key: normalize(item) for key, item in value.items()}
        item_id = result.get("id")
        if isinstance(item_id, str):
            if item_id.startswith("resp_"):
                result["id"] = "resp_normalized"
            elif item_id.startswith("msg_"):
                result["id"] = "msg_normalized"
            elif item_id.startswith("rs_"):
                result["id"] = "rs_normalized"
        return result
    return value

cases = json.load(sys.stdin)
json.dump([
    normalize(_response_from_chat(
        case["response"], case["model"], set(case["custom_names"])
    ))
    for case in cases
], sys.stdout, ensure_ascii=False, separators=(",", ":"))
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
        .expect("spawn Python Chat projection oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("fixture JSON")
                .as_bytes(),
        )
        .expect("write projection fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python Chat projection oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");

    let mut rust = fixture
        .as_array()
        .expect("fixture cases")
        .iter()
        .map(|case| {
            let custom_names = case["custom_names"]
                .as_array()
                .expect("custom names")
                .iter()
                .map(|name| name.as_str().expect("custom name"))
                .collect::<Vec<_>>();
            let ids =
                ChatIds::new("resp_rust", "msg_rust", "rs_rust", "rs_late_rust").expect("Rust IDs");
            let mut response = response_from_chat(
                &case["response"],
                case["model"].as_str().expect("model"),
                &custom_names,
                &ids,
            )
            .expect("Rust projection")
            .response;
            normalize_generated_ids(&mut response);
            response
        })
        .collect::<Vec<_>>();
    let rust = Value::Array(std::mem::take(&mut rust));
    assert_eq!(rust, oracle);
}

#[test]
fn chat_stream_events_match_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = stream_fixtures();
    let script = r#"
import json, sys
from unittest.mock import patch
from easy_multi_provider import router

def normalize(value):
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, dict):
        result = {key: normalize(item) for key, item in value.items()}
        for key in ("id", "item_id"):
            item_id = result.get(key)
            if isinstance(item_id, str):
                if item_id.startswith("resp_"):
                    result[key] = "resp_normalized"
                elif item_id.startswith("msg_"):
                    result[key] = "msg_normalized"
                elif item_id.startswith("rs_"):
                    result[key] = "rs_normalized"
        return result
    return value

class Upstream:
    def __init__(self, chunks):
        self.chunks = chunks
    def __iter__(self):
        wire = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in self.chunks)
        wire += "data: [DONE]\n\n"
        return iter(wire.encode().splitlines(keepends=True))
    def close(self):
        pass

def project(chunks):
    with patch.object(router, "_request", return_value=Upstream(chunks)):
        raw = b"".join(router.stream_chat_completion(
            {"id": "test", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"},
            {"model": "test/model", "input": "hello", "stream": True},
            {"id": "test/model"}, {},
        )).decode()
    return normalize([
        json.loads(line[6:]) for line in raw.splitlines() if line.startswith("data: ")
    ])

json.dump([project(chunks) for chunks in json.load(sys.stdin)], sys.stdout,
          ensure_ascii=False, separators=(",", ":"))
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
        .expect("spawn Python Chat stream oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("stream fixture JSON")
                .as_bytes(),
        )
        .expect("write stream fixtures");
    let output = child.wait_with_output().expect("wait for stream oracle");
    assert!(
        output.status.success(),
        "Python Chat stream oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("stream oracle JSON");

    let rust = fixture
        .as_array()
        .expect("stream cases")
        .iter()
        .map(|chunks| {
            let ids = ChatIds::new("resp_rust", "msg_rust", "rs_rust", "rs_late_rust")
                .expect("Rust stream IDs");
            let mut stream = ChatStream::new("test/model", ids);
            let mut events = vec![stream.start_event().expect("start").value];
            for chunk in chunks.as_array().expect("chunks") {
                events.extend(
                    stream
                        .push(chunk, ChatFrame::Sse)
                        .expect("Rust stream chunk")
                        .into_iter()
                        .map(|event| event.value),
                );
            }
            stream.mark_done();
            events.extend(
                stream
                    .finish()
                    .expect("Rust stream terminal")
                    .into_iter()
                    .map(|event| event.value),
            );
            let mut value = Value::Array(events);
            normalize_generated_ids(&mut value);
            value
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}

#[test]
fn responses_request_projection_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let fixture = request_fixtures();
    let script = r#"
import json, sys
from easy_multi_provider.protocol_projection import responses_to_chat
cases = json.load(sys.stdin)
json.dump([
    responses_to_chat(case["body"], case["model"]) for case in cases
], sys.stdout, ensure_ascii=False, separators=(",", ":"))
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
        .expect("spawn Python Chat request oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_string(&fixture)
                .expect("request fixture JSON")
                .as_bytes(),
        )
        .expect("write request fixtures");
    let output = child.wait_with_output().expect("wait for request oracle");
    assert!(
        output.status.success(),
        "Python Chat request oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("request oracle JSON");
    let rust = fixture
        .as_array()
        .expect("request cases")
        .iter()
        .map(|case| {
            responses_to_chat(
                &case["body"],
                case["model"].as_str().expect("upstream model"),
            )
            .expect("Rust request projection")
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}
