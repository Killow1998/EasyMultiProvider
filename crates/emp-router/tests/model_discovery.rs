//! Model discovery against live HTTP upstreams: what the user sees in the
//! model catalog when EMP lists a provider's models, plus the bounds that
//! keep a hostile catalog from exhausting memory.

use emp_router::discovery::{
    discover_gemini_models, discover_generic_models, discover_models, model_metadata,
    project_anthropic_models, project_gemini_models, project_generic_models,
};
use emp_transport::{HttpClient, HttpClientConfig, HttpClientPolicy};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

fn generic_fixture() -> Value {
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
            "nextPageToken": "next-page"
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

#[test]
fn generic_projection_drops_invalid_entries_and_normalizes_levels() {
    let models = project_generic_models(generic_fixture().as_object().expect("fixture object"))
        .expect("generic projection");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["upstream_id"], "demo-a");
    assert_eq!(models[0]["reasoning_levels"], json!(["low", "high"]));
    assert_eq!(models[0]["capabilities"]["streaming"], true);
    assert_eq!(models[1]["upstream_id"], "vendor/model:free");
    assert_eq!(models[1]["reasoning_levels"], json!([]));
    assert_eq!(
        models[1]["context_window"], 0,
        "context windows beyond the 100M cap are treated as unknown, not trusted"
    );
}

#[test]
fn hostile_catalogs_fail_closed_instead_of_exhausting_memory() {
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

#[test]
fn provider_specific_projections_normalize_gemini_and_anthropic_pages() {
    let mut gemini = Vec::new();
    for page in &gemini_pages() {
        gemini.extend(
            project_gemini_models(page.as_object().expect("Gemini page object"))
                .expect("Gemini projection"),
        );
    }
    let mut anthropic = Vec::new();
    for page in &anthropic_pages() {
        anthropic.extend(
            project_anthropic_models(page.as_object().expect("Anthropic page object"))
                .expect("Anthropic projection"),
        );
    }
    assert_eq!(gemini.len(), 2);
    assert_eq!(gemini[0]["upstream_id"], "gemini-test");
    assert_eq!(gemini[0]["input_modalities"], json!(["text", "image"]));
    assert_eq!(gemini[0]["reasoning_levels"], json!(["low", "high"]));
    assert_eq!(anthropic.len(), 2);
    assert_eq!(anthropic[0]["input_modalities"], json!(["text", "pdf"]));
    assert_eq!(anthropic[0]["reasoning_levels"], json!(["low", "high"]));
    assert_eq!(
        anthropic[1]["input_modalities"],
        json!(["text", "image"]),
        "a second-page model that advertises image_input gets the image modality"
    );
    assert_eq!(
        anthropic[1]["capabilities"],
        Value::Null,
        "capabilities are only projected for advertised structured outputs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_requests_carry_auth_headers_and_follow_pagination() {
    let gemini_fixture = gemini_pages();
    let anthropic_fixture = anthropic_pages();
    let gemini_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind gemini upstream");
    let gemini_address = gemini_listener.local_addr().expect("gemini address");
    let anthropic_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind anthropic upstream");
    let anthropic_address = anthropic_listener.local_addr().expect("anthropic address");

    let gemini_worker = thread::spawn(move || {
        for page in &gemini_fixture {
            let (mut stream, _) = gemini_listener.accept().expect("accept gemini page");
            let mut wire = Vec::new();
            loop {
                if wire.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).expect("read gemini request");
                assert_ne!(count, 0, "gemini request ended before headers");
                wire.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(wire).expect("ASCII gemini request");
            let path = request
                .split_whitespace()
                .nth(1)
                .expect("gemini request line")
                .to_owned();
            let lowered = request.to_ascii_lowercase();
            assert!(
                lowered.contains("x-goog-api-key: gemini-key\r\n"),
                "the provider key travels in the Gemini header, not a bearer"
            );
            assert!(!lowered.contains("authorization:"));
            let body = serde_json::to_vec(page).expect("serialize gemini page");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write gemini head");
            stream.write_all(&body).expect("write gemini page");
            if path.contains("pageToken=next-page") {
                break;
            }
        }
    });

    let anthropic_worker = thread::spawn(move || {
        for page in &anthropic_fixture {
            let (mut stream, _) = anthropic_listener.accept().expect("accept anthropic page");
            let mut wire = Vec::new();
            loop {
                if wire.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).expect("read anthropic request");
                assert_ne!(count, 0, "anthropic request ended before headers");
                wire.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(wire).expect("ASCII anthropic request");
            let path = request
                .split_whitespace()
                .nth(1)
                .expect("anthropic request line")
                .to_owned();
            let lowered = request.to_ascii_lowercase();
            assert!(lowered.contains("x-api-key: anthropic-key\r\n"));
            assert!(lowered.contains("anthropic-version: 2024-01-01\r\n"));
            let body = serde_json::to_vec(page).expect("serialize anthropic page");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write anthropic head");
            stream.write_all(&body).expect("write anthropic page");
            if path.contains("after_id=claude") {
                break;
            }
        }
    });

    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let gemini_provider = json!({
        "id": "gemini-fixture",
        "base_url": format!("http://{gemini_address}/v1beta/openai"),
        "protocol": "chat_completions",
        "auth_mode": "api_key",
        "api_key": "gemini-key"
    });
    // discover_gemini_models is called directly so the native Gemini auth
    // path (x-goog-api-key, no bearer) is exercised without pointing at the
    // real generativelanguage host, which discover_models requires by name.
    let models = discover_gemini_models(
        &client,
        gemini_provider.as_object().expect("Gemini provider"),
    )
    .await
    .expect("Gemini discovery");
    assert_eq!(models.len(), 2);

    let anthropic_provider = json!({
        "id": "anthropic-fixture",
        "base_url": format!("http://{anthropic_address}/v1/messages"),
        "protocol": "anthropic_messages",
        "auth_mode": "anthropic_api_key",
        "api_key": "anthropic-key",
        "anthropic_version": "2024-01-01"
    });
    let models = discover_models(
        &client,
        anthropic_provider.as_object().expect("Anthropic provider"),
    )
    .await
    .expect("Anthropic discovery");
    assert_eq!(models.len(), 2);

    gemini_worker.join().expect("join gemini upstream");
    anthropic_worker.join().expect("join anthropic upstream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_discovery_uses_bounded_native_transport() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind discovery upstream");
    let address = listener.local_addr().expect("discovery upstream address");
    let fixture = generic_fixture();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_without_usable_credentials_fails_before_any_network_call() {
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let provider = json!({
        "id": "demo", "base_url": "https://external.example/v1",
        "protocol": "chat_completions", "auth_mode": "api_key"
    });
    let error = discover_generic_models(&client, provider.as_object().expect("provider object"))
        .await
        .expect_err("missing key must fail");
    assert_eq!(error.status(), 503);

    let provider = json!({
        "id": "demo", "base_url": "https://external.example/v1",
        "protocol": "chat_completions", "auth_mode": "none"
    });
    let error = discover_generic_models(&client, provider.as_object().expect("provider object"))
        .await
        .expect_err("incompatible auth mode must fail");
    assert_eq!(error.status(), 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gemini_model_metadata_follows_same_origin_redirect_with_api_key() {
    let pages = vec![
        (
            302u16,
            Some("/v1beta/models/gemini-redirected".to_owned()),
            Value::Null,
        ),
        (
            200u16,
            None,
            json!({"inputTokenLimit": 32768, "outputTokenLimit": 8192}),
        ),
    ];
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind metadata upstream");
    let address = listener.local_addr().expect("metadata address");
    let worker = thread::spawn(move || {
        for (status, location, body) in pages {
            let (mut stream, _) = listener.accept().expect("accept metadata request");
            let mut wire = Vec::new();
            loop {
                if wire.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
                let mut buffer = [0_u8; 4096];
                let count = stream.read(&mut buffer).expect("read metadata request");
                assert_ne!(count, 0, "metadata request ended before headers");
                wire.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(wire).expect("ASCII metadata request");
            let lowered = request.to_ascii_lowercase();
            assert!(lowered.contains("x-goog-api-key: metadata-key\r\n"));
            if let Some(location) = location {
                write!(
                    stream,
                    "HTTP/1.1 {status} Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("write redirect headers");
            } else {
                let encoded = serde_json::to_vec(&body).expect("serialize metadata");
                write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    encoded.len()
                )
                .expect("write metadata head");
                stream.write_all(&encoded).expect("write metadata body");
            }
        }
    });

    let mut config = HttpClientConfig::default();
    config
        .add_dns_override("generativelanguage.googleapis.com", address)
        .expect("Gemini metadata DNS override");
    let client = HttpClient::with_config(HttpClientPolicy::default(), config).expect("HTTP client");
    let provider = json!({
        "id": "gemini-metadata-fixture",
        "base_url": format!(
            "http://generativelanguage.googleapis.com:{}/v1beta/openai",
            address.port()
        ),
        "api_key": "metadata-key"
    });
    let metadata = model_metadata(
        &client,
        provider.as_object().expect("provider object"),
        "gemini-redirect-test",
    )
    .await
    .expect("same-origin metadata redirect");
    assert_eq!(metadata["input_token_limit"], 32768);
    assert_eq!(metadata["output_token_limit"], 8192);
    worker.join().expect("join metadata upstream");
}
