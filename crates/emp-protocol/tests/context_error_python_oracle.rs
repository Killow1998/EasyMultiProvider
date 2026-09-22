use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};

use emp_protocol::context_error::is_explicit_context_error;

#[test]
fn explicit_context_error_matches_existing_guard_regressions() {
    let cases = [
        (
            400u16,
            "application/json",
            br#"{"error":{"code":"context_length_exceeded","message":"input too long"}}"#
                .as_slice(),
        ),
        (
            400,
            "application/json",
            br#"{"message":"maximum context length is 4096 tokens"}"#,
        ),
        (
            400,
            "text/html",
            b"<html>context length exceeded secret-body</html>",
        ),
        (
            403,
            "application/json",
            br#"{"message":"WAF denied request"}"#,
        ),
        (
            403,
            "application/json",
            br#"{"error":{"code":"context_length_exceeded"}}"#,
        ),
        (
            500,
            "application/json",
            br#"{"message":"server context length exceeded"}"#,
        ),
    ];

    let expected = [true, true, false, false, false, false];
    for ((status, media, raw), want) in cases.iter().zip(expected.iter()) {
        assert_eq!(
            is_explicit_context_error(*status, media, raw),
            *want,
            "status {status}, media {media}, raw {}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn explicit_context_error_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; context-error live oracle skipped");
        return;
    };

    let mut cases = json!([
        [
            200,
            "application/json",
            "{\"message\":\"context length exceeded\"}"
        ],
        [
            400,
            "application/json",
            "{\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"input too long\"}}"
        ],
        [
            400,
            "APPLICATION/JSON",
            "{\"message\":\"Maximum Context Length is 4096 tokens\"}"
        ],
        [
            400,
            "application/json;charset=utf-8",
            "{\"error\":{\"message\":\"prompt is too long\"}}"
        ],
        [400, "", "{\"param\":\"input-too-long\"}"],
        [400, "", "  {\"reason\":\"input  too long\"}"],
        [
            400,
            "",
            "[{\"error\":{\"message\":\"maximum context length exceeded\"}}]"
        ],
        [
            400,
            "text/html",
            "{\"message\":\"context length exceeded\"}"
        ],
        [400, "text/html", "<html>context length exceeded</html>"],
        [400, "application/json", "{\"message\":\"input too long\"}"],
        [400, "application/json", "{\"message\":\"too long input\"}"],
        [
            400,
            "application/json",
            "{\"message\":\"context exceeded length\"}"
        ],
        [400, "application/json", "{\"message\":\"too many tokens\"}"],
        [
            400,
            "application/json",
            "{\"message\":\"context length\\nexceeded\"}"
        ],
        [
            400,
            "application/json",
            "{\"message\":\"maximum\\ncontext\\nlength\"}"
        ],
        [
            400,
            "application/json",
            "{\"error\":{\"message\":[\"maximum context length\",\"now\"]}}"
        ],
        [
            400,
            "application/json",
            "{\"error\":{\"message\":{\"maximum context length\":\"now\"}}}"
        ],
        [
            400,
            "application/json",
            "{\"error\":{\"message\":{\"x\":\"maximum context length\"}}}"
        ],
        [
            400,
            "application/json",
            "{\"code\":{\"x\":\"input too long\"}}"
        ],
        [
            400,
            "application/json",
            "{\"error\":{\"detail\":{\"code\":\"context_length_exceeded\"}}}"
        ],
        [
            403,
            "application/json",
            "{\"error\":{\"code\":\"context_length_exceeded\"}}"
        ],
        [
            413,
            "application/json",
            "{\"error\":{\"code\":\"context_length_exceeded\"}}"
        ],
        [
            422,
            "application/json",
            "{\"error\":{\"code\":\"context_length_exceeded\"}}"
        ],
        [
            429,
            "application/json",
            "{\"error\":{\"code\":\"context_length_exceeded\"}}"
        ],
        [
            500,
            "application/json",
            "{\"message\":\"server context length exceeded\"}"
        ],
        [400, "application/json", "not-json"],
        [400, "application/json", "{\"message\":true}"],
        [
            400,
            "application/json",
            "{\"message\":[\"maximum context length\",null,3.5,false]}"
        ]
    ]);

    for message in [
        "context length maximal",
        "maximal context length",
        "context length max_",
        "context length maxé",
        "context length too   long",
        "context length too\tlong",
        "context length too\nlong",
        "context length length 12345678901234567890123456 exceeded",
        "context 中文中文中文中文中文中文中文中文 length exceeded",
    ] {
        cases.as_array_mut().unwrap().push(json!([
            400,
            "application/json",
            json!({"message": message}).to_string()
        ]));
    }
    let script = r#"
import json, sys
from easy_multi_provider.context_guard import is_explicit_context_error
json.dump([is_explicit_context_error(status, media, raw.encode("utf-8"))
           for status, media, raw in json.load(sys.stdin)], sys.stdout)
"#;
    let mut child = Command::new(python)
        .args(["-c", script])
        .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python context-error oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(&serde_json::to_vec(&cases).unwrap())
        .expect("write context-error fixtures");
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python context-error oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<bool> = serde_json::from_slice(&output.stdout).expect("oracle JSON");

    let actual = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            is_explicit_context_error(
                case[0].as_u64().unwrap().try_into().unwrap(),
                case[1].as_str().unwrap(),
                case[2].as_str().unwrap().as_bytes(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert!(actual.contains(&true));
    assert!(actual.contains(&false));
}
