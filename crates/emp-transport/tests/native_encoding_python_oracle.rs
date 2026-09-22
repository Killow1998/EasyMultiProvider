use emp_transport::{decode_content, zstd_encode};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

fn fixtures() -> Vec<Vec<u8>> {
    vec![
        Vec::new(),
        r#"{"message":"签名请求","count":1}"#.as_bytes().to_vec(),
        vec![b'n'; 256 * 1024],
        b"native zstd request framing ".repeat(48),
    ]
}

fn byte_array(value: &[u8]) -> Value {
    Value::Array(value.iter().copied().map(Value::from).collect())
}

#[test]
fn native_zstd_matches_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        eprintln!("EMP_PYTHON_INTEROP is unset; native zstd live oracle skipped");
        return;
    };
    let fixtures = fixtures();
    let compressed = fixtures
        .iter()
        .map(|raw| zstd_encode(raw.as_slice()).expect("Rust native zstd encoding"))
        .collect::<Vec<_>>();
    let script = r#"
import json, sys
from easy_multi_provider.transport import decode_content, zstd_encode

payload = json.load(sys.stdin)
fixtures = [bytes(case) for case in payload["compressed"]]
raw = payload["raw"]
results = {
    "rust_decode": [list(decode_content(case, "zstd", 1024 * 1024)) for case in fixtures],
    "python_compressed": [list(zstd_encode(case)) for case in map(bytes, raw)],
}
json.dump(results, sys.stdout, separators=(",", ":"))
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
        .expect("spawn Python zstd oracle");
    child
        .stdin
        .take()
        .expect("Python stdin")
        .write_all(
            serde_json::to_vec(&json!({
                "compressed": compressed,
                "raw": fixtures,
            }))
            .expect("serialize zstd fixtures")
            .as_slice(),
        )
        .expect("write zstd fixtures");
    let output = child
        .wait_with_output()
        .expect("wait for Python zstd oracle");
    assert!(
        output.status.success(),
        "Python zstd oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).expect("zstd oracle JSON");
    for (index, raw) in fixtures.iter().enumerate() {
        let rust_decoded = decode_content(compressed[index].clone(), "zstd", 1_048_576, None)
            .expect("Rust decoder accepts its own zstd frame");
        assert_eq!(rust_decoded, *raw);
        assert_eq!(
            byte_array(&rust_decoded),
            oracle["rust_decode"][index],
            "Python decoder must accept Rust zstd bytes for fixture {index}"
        );

        let python_compressed = oracle["python_compressed"][index]
            .as_array()
            .expect("Python compressed byte array")
            .iter()
            .map(|byte| byte.as_u64().expect("byte") as u8)
            .collect::<Vec<_>>();
        let rust_decode_of_python = decode_content(python_compressed, "zstd", 1_048_576, None)
            .expect("Rust decoder accepts Python zstd bytes");
        assert_eq!(rust_decode_of_python, *raw);
    }
}
