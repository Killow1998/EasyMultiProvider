use emp_router::discovery::{discover_generic_models, project_generic_models};
use emp_transport::{HttpClient, HttpClientPolicy};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

fn fixture() -> Value {
    json!({
        "data": [
            {
                "id": "demo-a",
                "name": "Demo A",
                "description": "advertised model",
                "context_length": 128000,
                "max_input_tokens": 120000,
                "top_provider": {"max_completion_tokens": 8192},
                "architecture": {
                    "input_modalities": ["text", "image", "text"],
                    "output_modalities": ["text"],
                    "supports_image_detail_original": true
                },
                "supported_parameters": [
                    "tools", "parallel_tool_calls", "response_format",
                    "reasoning_effort", "reasoning_summary"
                ],
                "reasoning_levels": ["high", "low", "low"],
                "streaming": true,
                "created": 1_735_689_600_000_u64
            },
            {
                "id": "models/vendor/model:free",
                "display_name": "Fallbacks",
                "context_window": 100_000_001,
                "output_limit": -1,
                "supports_reasoning": false,
                "reasoning_levels": [0, "high"],
                "architecture": {
                    "input_modalities": [],
                    "output_modalities": "text"
                },
                "streaming": false,
                "created_at": "2025-01-01T00:00:00Z"
            },
            {"id": null},
            {"id": "space is invalid"},
            "not-an-object"
        ]
    })
}

fn python_oracle(python: &str, payload: &Value) -> Value {
    let script = r#"
import json
import sys

from easy_multi_provider.model_discovery import DiscoveryIO, _generic_models

payload = json.load(sys.stdin)

class Response:
    def __enter__(self):
        return self
    def __exit__(self, *_args):
        return False

response = Response()
response.body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
io = DiscoveryIO(
    open_url=lambda *_args, **_kwargs: response,
    read_limited=lambda value, *_args, **_kwargs: value.body,
    http_error_message=lambda *_args: "error",
    discovery_headers=lambda *_args: {},
    anthropic_headers=lambda *_args: {},
)
provider = {
    "id": "demo",
    "base_url": "https://example.invalid/v1/responses",
    "protocol": "chat_completions",
    "auth_mode": "api_key",
    "api_key": "test-key",
}
json.dump(_generic_models(io, provider), sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python discovery oracle");
    serde_json::to_writer(child.stdin.as_mut().expect("Python oracle stdin"), payload)
        .expect("write Python discovery fixture");
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .expect("wait for Python discovery oracle");
    assert!(
        output.status.success(),
        "Python discovery oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python discovery JSON")
}

#[test]
fn generic_projection_matches_live_python() {
    let payload = fixture();
    let rust = Value::Array(
        project_generic_models(payload.as_object().expect("fixture object"))
            .expect("Rust generic projection"),
    );
    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        assert_eq!(rust, python_oracle(&python, &payload));
    }
    assert_eq!(rust.as_array().map(Vec::len), Some(2));
    assert_eq!(rust[0]["upstream_id"], "demo-a");
    assert_eq!(rust[0]["reasoning_levels"], json!(["low", "high"]));
    assert_eq!(rust[1]["upstream_id"], "vendor/model:free");
    assert_eq!(rust[1]["reasoning_levels"], json!([]));
}

#[test]
fn generic_projection_enforces_python_bounds() {
    let oversized = "x".repeat(4097);
    let value = json!({"data": [{"id": "valid", "description": oversized}]});
    let error = project_generic_models(value.as_object().expect("object"))
        .expect_err("oversized field must fail");
    assert_eq!(error.status(), 502);

    let models = (0..1001)
        .map(|index| json!({"id": format!("model-{index}")}))
        .collect::<Vec<_>>();
    let value = json!({"data": models});
    let error = project_generic_models(value.as_object().expect("object"))
        .expect_err("oversized catalog must fail");
    assert_eq!(error.status(), 502);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_discovery_uses_bounded_native_transport() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind discovery upstream");
    let address = listener.local_addr().expect("discovery upstream address");
    let fixture = fixture();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept discovery request");
        let mut wire = Vec::new();
        let header_end = loop {
            if let Some(position) = wire.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
            let mut buffer = [0_u8; 4096];
            let count = stream.read(&mut buffer).expect("read discovery request");
            assert_ne!(count, 0, "discovery request ended before headers");
            wire.extend_from_slice(&buffer[..count]);
        };
        let request = String::from_utf8(wire[..header_end].to_vec()).expect("ASCII headers");
        let lowered = request.to_ascii_lowercase();
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(lowered.contains("authorization: bearer test-key\r\n"));
        assert!(lowered.contains("accept: application/json\r\n"));
        let body = serde_json::to_vec(&fixture).expect("serialize discovery fixture");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write discovery response head");
        stream.write_all(&body).expect("write discovery response");
    });

    let provider = json!({
        "id": "demo",
        "base_url": format!("http://{address}/v1/responses"),
        "protocol": "chat_completions",
        "auth_mode": "api_key",
        "api_key": "test-key"
    });
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let models = discover_generic_models(&client, provider.as_object().expect("provider object"))
        .await
        .expect("generic discovery");
    server.join().expect("join discovery upstream");
    assert_eq!(models.len(), 2);
}

#[test]
fn generic_discovery_rejects_missing_or_incompatible_credentials() {
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let runtime = tokio::runtime::Runtime::new().expect("Tokio runtime");
    let provider = json!({
        "id": "demo", "base_url": "https://example.invalid/v1",
        "protocol": "chat_completions", "auth_mode": "api_key"
    });
    let error = runtime
        .block_on(discover_generic_models(
            &client,
            provider.as_object().expect("provider object"),
        ))
        .expect_err("missing key must fail");
    assert_eq!(error.status(), 503);

    let provider = json!({
        "id": "demo", "base_url": "https://example.invalid/v1",
        "protocol": "chat_completions", "auth_mode": "none"
    });
    let error = runtime
        .block_on(discover_generic_models(
            &client,
            provider.as_object().expect("provider object"),
        ))
        .expect_err("incompatible auth mode must fail");
    assert_eq!(error.status(), 400);
}
