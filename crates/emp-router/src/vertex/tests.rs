use super::*;
use emp_domain::{RouteSource, SignatureStore};
use emp_protocol::gemini;
use serde_json::json;
use std::sync::Arc;

fn route(
    mode: &str,
    protocol: Protocol,
    model: &str,
    location: &str,
    project: &str,
) -> ResolvedRoute {
    let provider = json!({"id":"vertex", "base_url":"https://aiplatform.googleapis.com/v1",
        "auth_mode":mode, "api_key":"fixture", "vertex":{"project":project,"location":location}});
    ResolvedRoute::new(
        "vertex/model",
        model,
        RouteSource::ExplicitModel,
        provider.as_object().unwrap().clone(),
        json!({"id":"vertex/model","upstream_id":model})
            .as_object()
            .unwrap()
            .clone(),
        protocol,
        if protocol == Protocol::AnthropicMessages {
            Dialect::AnthropicMessages
        } else {
            Dialect::ChatCompletions
        },
        "vertex",
        format!("sha256:{}", "1".repeat(64)),
        "default",
    )
    .unwrap()
}

#[test]
fn google_and_claude_use_their_documented_paths() {
    let gemini = route(
        "vertex_adc",
        Protocol::ChatCompletions,
        "gemini-3-flash-preview",
        "global",
        "fixture-project",
    );
    assert_eq!(
        endpoint(&gemini, true).unwrap(),
        "https://aiplatform.googleapis.com/v1/projects/fixture-project/locations/global/endpoints/openapi/chat/completions"
    );
    let claude = route(
        "vertex_adc",
        Protocol::AnthropicMessages,
        "claude-3-5-sonnet-v2@20241022",
        "us-east5",
        "fixture-project",
    );
    assert_eq!(
        endpoint(&claude, true).unwrap(),
        "https://us-east5-aiplatform.googleapis.com/v1/projects/fixture-project/locations/us-east5/publishers/anthropic/models/claude-3-5-sonnet-v2@20241022:streamRawPredict"
    );
    assert!(endpoint(&claude, false).unwrap().ends_with(":rawPredict"));
    let express = route(
        "vertex_api_key",
        Protocol::ChatCompletions,
        "gemini-3-flash-preview",
        "global",
        "",
    );
    assert_eq!(
        endpoint(&express, true).unwrap(),
        "https://aiplatform.googleapis.com/v1/publishers/google/models/gemini-3-flash-preview:streamGenerateContent?alt=sse"
    );
    let project_key = route(
        "vertex_api_key",
        Protocol::ChatCompletions,
        "gemini-3-flash-preview",
        "us-central1",
        "fixture-project",
    );
    assert_eq!(
        endpoint(&project_key, false).unwrap(),
        "https://us-central1-aiplatform.googleapis.com/v1/projects/fixture-project/locations/us-central1/publishers/google/models/gemini-3-flash-preview:generateContent"
    );
    assert!(
        endpoint(
            &route(
                "vertex_api_key",
                Protocol::AnthropicMessages,
                "claude-opus-5-5",
                "global",
                "fixture-project"
            ),
            true
        )
        .is_err()
    );
}

#[test]
fn signatures_survive_restart_and_are_scoped_to_the_project_and_model() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("signatures.sqlite3");
    let route = route(
        "vertex_adc",
        Protocol::ChatCompletions,
        "gemini-3-flash-preview",
        "global",
        "fixture-project",
    );
    let store: Arc<dyn SignatureStore> = Arc::new(
        emp_storage::gemini_signatures::GeminiSignatures::new(path.clone()),
    );
    let context = GeminiContext::for_route(&route, Some(&store))
        .unwrap()
        .unwrap();
    context.remember(&json!({"call_id":"call_1","extra_content":{"google":{"thought_signature":"TEST_SIGNATURE"}}})).unwrap();
    drop(context);
    drop(store);
    let reopened: Arc<dyn SignatureStore> =
        Arc::new(emp_storage::gemini_signatures::GeminiSignatures::new(path));
    let context = GeminiContext::for_route(&route, Some(&reopened))
        .unwrap()
        .unwrap();
    let mut chat = json!({"messages":[{"role":"assistant","tool_calls":[{"id":"call_1"}]},{"role":"assistant","tool_calls":[{"id":"foreign"}]}]});
    context.restore(&mut chat).unwrap();
    assert_eq!(
        chat["messages"][0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "TEST_SIGNATURE"
    );
    assert_eq!(
        chat["messages"][1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "skip_thought_signature_validator"
    );
}

#[test]
fn gemini_request_keeps_images_tools_and_parallel_result_order() {
    let chat = json!({"model":"google/gemini-3-flash-preview","messages":[
        {"role":"system","content":"system"},
        {"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}}]},
        {"role":"assistant","content":null,"tool_calls":[
            {"id":"call_1","function":{"name":"read","arguments":"{\"path\":\"one\"}"},"extra_content":{"google":{"thought_signature":"TEST_SIGNATURE"}}},
            {"id":"call_2","function":{"name":"read","arguments":"{\"path\":\"two\"}"}}]},
        {"role":"tool","tool_call_id":"call_2","content":"second"},
        {"role":"tool","tool_call_id":"call_1","content":"first"}],
        "tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}],
        "max_tokens":100,"reasoning_effort":"low"});
    let body = gemini_request(&chat, None, &emp_config::official_registry::snapshot()).unwrap();
    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "system");
    assert_eq!(
        body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["thoughtSignature"],
        "TEST_SIGNATURE"
    );
    assert_eq!(
        body["contents"][1]["parts"][0]["functionCall"]["name"],
        "read"
    );
    assert!(
        body["contents"][1]["parts"][0]["functionCall"]
            .get("id")
            .is_none()
    );
    assert_eq!(
        body["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
        "first"
    );
    assert_eq!(
        body["contents"][2]["parts"][1]["functionResponse"]["response"]["result"],
        "second"
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"],
        "object"
    );
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "LOW"
    );
    let carbon = json!({"model":"gemini-4-carbon","reasoning_effort":"high","messages":[{"role":"user","content":"hello"}]});
    assert_eq!(
        gemini_request(&carbon, None, &emp_config::official_registry::snapshot()).unwrap()["generationConfig"]
            ["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    for (model, expected) in [
        ("gemini-4-hydrogen", "XHIGH"),
        ("google/gemini-4-hydrogen", "XHIGH"),
        ("gemini-4-carbon", "HIGH"),
        ("gemini-3-flash-preview", "HIGH"),
    ] {
        let request = json!({"model":model,"reasoning_effort":"xhigh","messages":[{"role":"user","content":"hello"}]});
        assert_eq!(
            gemini_request(&request, None, &emp_config::official_registry::snapshot()).unwrap()["generationConfig"]
                ["thinkingConfig"]["thinkingLevel"],
            expected,
            "{model}"
        );
    }
}

#[test]
fn gemini_reported_version_survives_complete_and_metadata_only_stream_frames() {
    let version = "gemini-3-flash-preview-actual";
    let mut response = json!({"modelVersion":version,"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]});
    let complete = gemini::complete(&response, "resp_model").unwrap();
    assert_eq!(complete["model"], version);
    let mut observation = crate::model_observation::ModelObservation::default();
    observation.observe(&complete, false);
    assert_eq!(
        observation.project("gemini-3-flash-preview")["response_model"],
        version
    );
    response.as_object_mut().unwrap().remove("modelVersion");
    let mut stream = gemini::Stream::new("resp_model");
    let mut observation = crate::model_observation::ModelObservation::default();
    for value in [json!({"modelVersion":version}), response] {
        for chunk in stream.push(&value).unwrap() {
            observation.observe(&chunk, true);
        }
    }
    let facts = observation.project("gemini-3-flash-preview");
    assert_eq!(facts["response_model"], version);
    assert_ne!(facts["model_name_status"], "missing");
}
