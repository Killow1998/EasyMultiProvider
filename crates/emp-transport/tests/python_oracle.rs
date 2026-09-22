use emp_transport::{SseJsonParser, sse_json_events};
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
