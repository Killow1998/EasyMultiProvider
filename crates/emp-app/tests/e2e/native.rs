use crate::support::{Emp, HttpsProxy, Upstream, Workspace, write_file};
use serde_json::{Value, json};

fn reply(upstream: &Upstream, wire: &str, streaming: bool, answer: &str) {
    let usage = json!({"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5});
    let complete = match wire {
        "gemini" => json!({
            "modelVersion": "gemini-3-flash-preview", "candidates": [{
                "content": {"role": "model", "parts": [{"text": answer}]}, "finishReason": "STOP",
            }], "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5},
        }),
        "chat" => json!({
            "id": "chat_vertex", "object": "chat.completion", "model": "google/gemini-3-flash-preview",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": answer}, "finish_reason": "stop"}],
            "usage": usage,
        }),
        "anthropic" => json!({
            "id": "msg_vertex", "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
            "content": [{"type": "text", "text": answer}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2},
        }),
        _ => unreachable!("fixture wire protocol"),
    };
    if !streaming {
        upstream.reply_json(&complete);
        return;
    }
    let events = match wire {
        "gemini" => vec![complete],
        "chat" => vec![
            json!({"id": "chat_vertex", "choices": [{"index": 0,
                "delta": {"content": answer}, "finish_reason": null}]}),
            json!({"id": "chat_vertex", "choices": [{"index": 0,
                "delta": {}, "finish_reason": "stop"}], "usage": usage}),
        ],
        "anthropic" => vec![
            json!({"type": "message_start", "message": {"id": "msg_vertex",
                "type": "message", "role": "assistant", "model": "claude-sonnet-4-6",
                "content": [], "usage": {"input_tokens": 3, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": answer}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ],
        _ => unreachable!("fixture wire protocol"),
    };
    let mut body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    if wire == "chat" {
        body.push_str("data: [DONE]\n\n");
    }
    upstream.configure(200, "text/event-stream", body);
}

#[test]
fn gemini_and_vertex_complete_and_sse_keep_paths_auth_models_text_and_usage() {
    let upstream = Upstream::start();
    let oauth = Upstream::start();
    oauth.reply_json(
        &json!({"access_token": "fixture-oauth-token", "token_type": "Bearer", "expires_in": 3600}),
    );
    let workspace = Workspace::new();
    std::fs::create_dir(workspace.root.join("empty-certs"))
        .expect("isolated certificate directory");
    let proxy = HttpsProxy::start(&workspace.root, &upstream);
    let credentials = workspace.root.join("fake-adc.json");
    write_file(
        &credentials,
        &json!({
            "type": "authorized_user", "client_id": "fixture-id", "client_secret": "fixture-secret",
            "refresh_token": "fixture-refresh", "token_uri": format!("{}/token", oauth.base_url()),
        })
        .to_string(),
    );
    let variants = [
        (
            "express",
            "vertex_api_key",
            "",
            "global",
            "gemini",
            "gemini-3-flash-preview",
            "aiplatform.googleapis.com:443",
            "/v1/publishers/google/models/gemini-3-flash-preview",
        ),
        (
            "regional",
            "vertex_api_key",
            "fixture-project",
            "us-central1",
            "gemini",
            "gemini-3-flash-preview",
            "us-central1-aiplatform.googleapis.com:443",
            "/v1/projects/fixture-project/locations/us-central1/publishers/google/models/gemini-3-flash-preview",
        ),
        (
            "adc-gemini",
            "vertex_adc",
            "fixture-project",
            "global",
            "chat",
            "gemini-3-flash-preview",
            "aiplatform.googleapis.com:443",
            "/v1/projects/fixture-project/locations/global/endpoints/openapi/chat/completions",
        ),
        (
            "adc-claude",
            "vertex_adc",
            "fixture-project",
            "global",
            "anthropic",
            "claude-sonnet-4-6",
            "aiplatform.googleapis.com:443",
            "/v1/projects/fixture-project/locations/global/publishers/anthropic/models/claude-sonnet-4-6",
        ),
    ];
    let providers: Vec<_> = variants
        .iter()
        .map(|(id, mode, project, location, _, _, _, _)| {
            json!({
                "id": id, "name": id, "protocol": "auto", "auth_mode": mode,
                "api_key": if *mode == "vertex_api_key" {"fixture-google-key"} else {""},
                "vertex": {"project": project, "location": location},
            })
        })
        .collect();
    let models: Vec<_> = variants
        .iter()
        .map(|(id, _, _, _, _, model, _, _)| {
            json!({
                "id": format!("{id}/model"), "provider": id, "upstream_id": model, "enabled": true,
            })
        })
        .collect();
    workspace.write_config(json!({"providers": providers, "models": models,
        "codex_base_url": upstream.base_url()}));
    let mut command = workspace.command();
    command
        .args(workspace.serve_arguments())
        .env("PATH", workspace.root.join("empty-bin"))
        .env_remove("NVM_DIR")
        .env("HTTPS_PROXY", proxy.url())
        .env("SSL_CERT_FILE", &proxy.certificate)
        .env("SSL_CERT_DIR", workspace.root.join("empty-certs"))
        .env("GOOGLE_APPLICATION_CREDENTIALS", &credentials)
        .env_remove("GOOGLE_CLOUD_QUOTA_PROJECT");
    let emp = Emp::spawn(command);
    for (id, mode, _, _, wire, model, host, prefix) in variants {
        for streaming in [false, true] {
            let answer = format!("{id} answer {streaming}");
            reply(&upstream, wire, streaming, &answer);
            let response = emp.responses(&json!({
                "model": format!("{id}/model"), "input": "Gemini and Vertex fixture prompt", "stream": streaming,
                "tools": [{"type": "function", "name": "echo", "parameters": {
                    "type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"],
                }}],
            }));
            assert_eq!(
                response.status,
                200,
                "{id}/{streaming}: {}",
                response.text()
            );
            assert_eq!(proxy.next_connect(), host);
            let request = upstream.next_forwarded();
            let suffix = match (wire, streaming) {
                ("gemini", false) => ":generateContent",
                ("gemini", true) => ":streamGenerateContent?alt=sse",
                ("anthropic", false) => ":rawPredict",
                ("anthropic", true) => ":streamRawPredict",
                _ => "",
            };
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, format!("{prefix}{suffix}"));
            if mode == "vertex_api_key" {
                assert_eq!(request.headers["x-goog-api-key"], "fixture-google-key");
                assert!(!request.headers.contains_key("authorization"));
            } else {
                assert_eq!(
                    request.headers["authorization"],
                    "Bearer fixture-oauth-token"
                );
                assert!(!request.headers.contains_key("x-goog-api-key"));
            }
            let body = request.body.expect("Google request JSON");
            assert!(
                body.to_string()
                    .contains("Gemini and Vertex fixture prompt"),
                "{body}"
            );
            match wire {
                "gemini" => {
                    assert!(body.get("model").is_none());
                    assert_eq!(body["tools"][0]["functionDeclarations"][0]["name"], "echo");
                    assert!(body["contents"].is_array());
                }
                "chat" => {
                    assert_eq!(body["model"], format!("google/{model}"));
                    assert_eq!(body["tools"][0]["function"]["name"], "echo");
                    assert_eq!(body["stream"], streaming);
                }
                "anthropic" => {
                    assert!(body.get("model").is_none());
                    assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
                    assert_eq!(body["tools"][0]["name"], "echo");
                    assert_eq!(body["stream"], streaming);
                }
                _ => unreachable!("fixture wire protocol"),
            }
            let result: Value = if streaming {
                let events = response.sse_events();
                let text: String = events
                    .iter()
                    .filter(|event| event["type"] == "response.output_text.delta")
                    .map(|event| event["delta"].as_str().expect("text delta"))
                    .collect();
                assert_eq!(text, answer);
                let completed = events.last().expect("terminal event");
                assert_eq!(completed["type"], "response.completed");
                completed["response"].clone()
            } else {
                let result = response.json();
                assert_eq!(result["output"][0]["content"][0]["text"], answer);
                result
            };
            assert_eq!(result["status"], "completed");
            assert_eq!(result["usage"]["input_tokens"], 3);
            assert_eq!(result["usage"]["output_tokens"], 2);
            assert_eq!(result["usage"]["total_tokens"], 5);
            upstream.assert_idle();
        }
    }
}
