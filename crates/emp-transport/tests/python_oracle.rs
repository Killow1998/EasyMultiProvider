use emp_transport::{
    MemoryStatus, RequestLimits, RequestLimitsConfig, SseJsonParser, TransportKind, sse_json_events,
};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn sse_parser_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let cases = json!([
        ["data: {\"type\":\"single\"}\n\n"],
        ["data: {\"type\":", "\"split\"}\r\n\r\n"],
        [": keepalive\ndata: {\"value\":\ndata: [1,2]}\n\n"],
        ["data: [DONE]\n\ndata: {\"type\":\"after_done\"}\n\n"],
        ["event: ignored\nid: 3\ndata: {\"type\":\"tail\"}"]
    ]);
    let script = r#"
import json, sys
from easy_multi_provider.transport import sse_json_events
cases = json.load(sys.stdin)
json.dump([list(sse_json_events([chunk.encode() for chunk in case])) for case in cases],
          sys.stdout, ensure_ascii=False, separators=(",", ":"))
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
        .expect("spawn Python SSE oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(serde_json::to_string(&cases).unwrap().as_bytes())
        .expect("write fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python SSE oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    let rust = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let chunks = case
                .as_array()
                .unwrap()
                .iter()
                .map(|chunk| chunk.as_str().unwrap().as_bytes());
            Value::Array(
                sse_json_events(chunks)
                    .expect("Rust projection")
                    .into_iter()
                    .map(Value::Object)
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(Value::Array(rust), oracle);
}

#[test]
fn split_multibyte_utf8_is_supported() {
    let wire = "data: {\"text\":\"思考\"}\n\n".as_bytes();
    let split = wire.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
    let mut parser = SseJsonParser::new();
    assert!(parser.push(&wire[..split]).unwrap().is_empty());
    let mut events = parser.push(&wire[split..]).unwrap();
    events.extend(parser.finish().unwrap());
    assert_eq!(
        events,
        vec![json!({"text": "思考"}).as_object().unwrap().clone()]
    );
}

#[test]
fn request_admission_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import json
from easy_multi_provider.request_limits import RequestLimits

limits = RequestLimits(
    baseline=64, maximum=256, available_memory=lambda: 2048,
    growth_quantum=64, memory_headroom=0,
)
first = limits.request("http")
second = limits.request("websocket")
first.ensure(64)
first.ensure(65)
first.ensure(129)
errors = []
try:
    second.ensure(65)
except Exception as exc:
    errors.append({
        "reason": exc.reason, "limit": exc.limit,
        "available_bytes": exc.available_bytes,
        "required_memory_bytes": exc.required_memory_bytes,
    })
first.release()
second.ensure(65)
third = limits.request("http")
try:
    third.ensure(257)
except Exception as exc:
    errors.append({
        "reason": exc.reason, "limit": exc.limit,
        "available_bytes": exc.available_bytes,
        "required_memory_bytes": exc.required_memory_bytes,
    })
snapshot = limits.snapshot()
snapshot["run_id"] = "run-test"
for notice in snapshot["notices"]:
    notice["timestamp"] = 0
print(json.dumps({"snapshot": snapshot, "errors": errors},
                 ensure_ascii=False, separators=(",", ":")))
"#;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .output()
        .expect("spawn Python admission oracle");
    assert!(
        output.status.success(),
        "Python admission oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle_line = output
        .stdout
        .split(|byte| *byte == b'\n')
        .rev()
        .find(|line| !line.is_empty())
        .expect("admission oracle output");
    let oracle: Value = serde_json::from_slice(oracle_line).expect("admission oracle JSON");

    let limits = RequestLimits::new(
        RequestLimitsConfig {
            baseline: 64,
            maximum: 256,
            growth_quantum: 64,
            memory_headroom: 0,
        },
        || Some(MemoryStatus::available(2048)),
        || 0,
        "run-test",
    )
    .unwrap();
    let mut first = limits.request(TransportKind::Http);
    let mut second = limits.request(TransportKind::WebSocket);
    first.ensure(64).unwrap();
    first.ensure(65).unwrap();
    first.ensure(129).unwrap();
    let memory = second.ensure(65).expect_err("memory pressure");
    first.release();
    second.ensure(65).unwrap();
    let mut third = limits.request(TransportKind::Http);
    let hard = third.ensure(257).expect_err("hard limit");
    let rust = json!({
        "snapshot": limits.snapshot().unwrap(),
        "errors": [
            {
                "reason": memory.reason.as_str(), "limit": memory.limit,
                "available_bytes": memory.available_bytes,
                "required_memory_bytes": memory.required_memory_bytes,
            },
            {
                "reason": hard.reason.as_str(), "limit": hard.limit,
                "available_bytes": hard.available_bytes,
                "required_memory_bytes": hard.required_memory_bytes,
            }
        ]
    });
    assert_eq!(rust, oracle);
}
