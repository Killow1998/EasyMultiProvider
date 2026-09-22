use emp_router::discovery::{
    discover_anthropic_models, discover_models, project_anthropic_models, project_gemini_models,
};
use emp_transport::{HttpClient, HttpClientConfig, HttpClientPolicy};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

fn gemini_pages() -> Vec<Value> {
    vec![
        json!({
            "models": [
                {
                    "name": "models/gemini-test",
                    "displayName": "Gemini Test",
                    "description": "native catalog",
                    "supportedGenerationMethods": ["generateContent", "countTokens"],
                    "inputModalities": ["TEXT", "IMAGE"],
                    "outputModalities": ["TEXT"],
                    "inputTokenLimit": 1_048_576,
                    "outputTokenLimit": 65_536,
                    "supported_parameters": ["reasoning_effort"],
                    "reasoning_levels": ["high", "low"],
                    "created": "2026-01-02T03:04:05Z"
                },
                {
                    "name": "models/text-embedding-test",
                    "supportedGenerationMethods": ["embedContent"]
                }
            ],
            "nextPageToken": "next /✓"
        }),
        json!({
            "models": [{
                "name": "models/gemini-second",
                "displayName": "Gemini Second",
                "supportedInputModalities": ["text"],
                "supportedOutputModalities": ["text"],
                "supports_reasoning": false
            }]
        }),
    ]
}

fn anthropic_pages() -> Vec<Value> {
    vec![
        json!({
            "data": [{
                "id": "claude-test",
                "display_name": "Claude Test",
                "created_at": "2026-02-03T04:05:06Z",
                "max_input_tokens": 200000,
                "max_tokens": 64000,
                "supports_reasoning_summaries": true,
                "capabilities": {
                    "thinking": {"supported": true},
                    "effort": {
                        "supported": true,
                        "low": {"supported": true},
                        "medium": {"supported": false},
                        "high": {"supported": true}
                    },
                    "image_input": {"supported": false},
                    "pdf_input": {"supported": true},
                    "structured_outputs": {"supported": false}
                }
            }],
            "has_more": true,
            "last_id": "claude next"
        }),
        json!({
            "data": [{
                "id": "claude-second",
                "capabilities": {
                    "thinking": {"supported": false},
                    "image_input": {"supported": true}
                }
            }],
            "has_more": false,
            "last_id": "claude-second"
        }),
    ]
}

fn python_oracle(python: &str, pages: &Value) -> Value {
    let script = r#"
import json
import sys

from easy_multi_provider.model_discovery import DiscoveryIO, discover_models

fixtures = json.load(sys.stdin)

class Response:
    def __init__(self, value):
        self.body = json.dumps(value, separators=(",", ":")).encode("utf-8")
    def __enter__(self):
        return self
    def __exit__(self, *_args):
        return False

def run(provider, pages):
    remaining = list(pages)
    def open_url(*_args, **_kwargs):
        return Response(remaining.pop(0))
    io = DiscoveryIO(
        open_url=open_url,
        read_limited=lambda response, *_args, **_kwargs: response.body,
        http_error_message=lambda *_args: "error",
        discovery_headers=lambda *_args: {},
        anthropic_headers=lambda *_args: {},
    )
    return discover_models(io, provider)

gemini = run({
    "id": "gemini-fixture",
    "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
    "protocol": "chat_completions",
    "auth_mode": "api_key",
    "api_key": "test-key",
}, fixtures["gemini"])
anthropic = run({
    "id": "anthropic-fixture",
    "base_url": "https://api.anthropic.com/v1/messages",
    "protocol": "anthropic_messages",
    "auth_mode": "anthropic_api_key",
    "api_key": "test-key",
}, fixtures["anthropic"])
json.dump({"gemini": gemini, "anthropic": anthropic}, sys.stdout, ensure_ascii=False, separators=(",", ":"))
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
        .expect("spawn Python provider discovery oracle");
    serde_json::to_writer(child.stdin.as_mut().expect("oracle stdin"), pages)
        .expect("write provider discovery fixtures");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python provider discovery oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python provider discovery JSON")
}

#[test]
fn provider_specific_projections_match_live_python() {
    let gemini_pages = gemini_pages();
    let anthropic_pages = anthropic_pages();
    let mut gemini = Vec::new();
    for page in &gemini_pages {
        gemini.extend(
            project_gemini_models(page.as_object().expect("Gemini page object"))
                .expect("Gemini projection"),
        );
    }
    let mut anthropic = Vec::new();
    for page in &anthropic_pages {
        anthropic.extend(
            project_anthropic_models(page.as_object().expect("Anthropic page object"))
                .expect("Anthropic projection"),
        );
    }
    let rust = json!({"gemini": gemini, "anthropic": anthropic});
    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        let fixtures = json!({"gemini": gemini_pages, "anthropic": anthropic_pages});
        assert_eq!(rust, python_oracle(&python, &fixtures));
    }
    assert_eq!(rust["gemini"].as_array().map(Vec::len), Some(2));
    assert_eq!(rust["anthropic"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        rust["anthropic"][0]["input_modalities"],
        json!(["text", "pdf"])
    );
    assert_eq!(
        rust["anthropic"][0]["reasoning_levels"],
        json!(["low", "high"])
    );
}

#[derive(Debug, Clone)]
struct RequestRecord {
    path: String,
    headers: BTreeMap<String, String>,
}

struct PageServer {
    address: SocketAddr,
    records: Arc<Mutex<Vec<RequestRecord>>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PageServer {
    fn start(pages: Vec<Value>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind discovery pages");
        let address = listener.local_addr().expect("discovery page address");
        let records = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let records = Arc::clone(&records);
            thread::spawn(move || {
                for page in pages {
                    let (mut stream, _) = listener.accept().expect("accept discovery page");
                    let mut wire = Vec::new();
                    let header_end = loop {
                        if let Some(position) = wire.windows(4).position(|part| part == b"\r\n\r\n")
                        {
                            break position + 4;
                        }
                        let mut buffer = [0_u8; 4096];
                        let count = stream.read(&mut buffer).expect("read discovery headers");
                        assert_ne!(count, 0, "request ended before headers");
                        wire.extend_from_slice(&buffer[..count]);
                    };
                    let head =
                        String::from_utf8(wire[..header_end].to_vec()).expect("ASCII headers");
                    let mut lines = head.split("\r\n");
                    let path = lines
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_owned();
                    let headers = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(name, value)| {
                            (name.trim().to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect();
                    records
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(RequestRecord { path, headers });
                    let body = serde_json::to_vec(&page).expect("serialize page");
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .expect("write page headers");
                    stream.write_all(&body).expect("write page body");
                }
            })
        };
        Self {
            address,
            records,
            thread: Some(thread),
        }
    }

    fn records(&self) -> Vec<RequestRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for PageServer {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join discovery page server");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_dispatch_and_anthropic_pagination_use_exact_headers_and_urls() {
    let gemini_server = PageServer::start(gemini_pages());
    let mut config = HttpClientConfig::default();
    config
        .add_dns_override("generativelanguage.googleapis.com", gemini_server.address)
        .expect("Gemini DNS override");
    let client = HttpClient::with_config(HttpClientPolicy::default(), config).expect("HTTP client");
    let gemini_provider = json!({
        "id": "gemini-fixture",
        "base_url": format!(
            "http://generativelanguage.googleapis.com:{}/v1beta/openai",
            gemini_server.address.port()
        ),
        "protocol": "chat_completions",
        "auth_mode": "api_key",
        "api_key": "gemini-key"
    });
    let models = discover_models(
        &client,
        gemini_provider.as_object().expect("Gemini provider"),
    )
    .await
    .expect("Gemini discovery");
    assert_eq!(models.len(), 2);
    let records = gemini_server.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].path, "/v1beta/models");
    assert_eq!(
        records[1].path,
        "/v1beta/models?pageToken=next%20%2F%E2%9C%93"
    );
    assert_eq!(
        records[0].headers.get("x-goog-api-key").map(String::as_str),
        Some("gemini-key")
    );
    assert!(!records[0].headers.contains_key("authorization"));
    drop(gemini_server);

    let anthropic_server = PageServer::start(anthropic_pages());
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let anthropic_provider = json!({
        "id": "anthropic-fixture",
        "base_url": format!("http://{}/v1/messages", anthropic_server.address),
        "protocol": "anthropic_messages",
        "auth_mode": "anthropic_api_key",
        "api_key": "anthropic-key",
        "anthropic_version": "2024-01-01"
    });
    let models = discover_anthropic_models(
        &client,
        anthropic_provider.as_object().expect("Anthropic provider"),
    )
    .await
    .expect("Anthropic discovery");
    assert_eq!(models.len(), 2);
    let records = anthropic_server.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].path, "/v1/models?limit=1000");
    assert_eq!(
        records[1].path,
        "/v1/models?limit=1000&after_id=claude%20next"
    );
    assert_eq!(
        records[0].headers.get("x-api-key").map(String::as_str),
        Some("anthropic-key")
    );
    assert_eq!(
        records[0]
            .headers
            .get("anthropic-version")
            .map(String::as_str),
        Some("2024-01-01")
    );
}
