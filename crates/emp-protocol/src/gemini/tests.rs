use crate::gemini;
use serde_json::json;

#[test]
fn gemini_output_usage_keeps_thought_only_zero_and_unknown_distinct() {
    for (usage, expected) in [
        (json!({}), None),
        (json!({"thoughtsTokenCount":7}), Some(7)),
        (json!({"thoughtsTokenCount":0}), Some(0)),
        (json!({"candidatesTokenCount":0}), Some(0)),
        (
            json!({"candidatesTokenCount":10,"thoughtsTokenCount":7}),
            Some(17),
        ),
    ] {
        let response = json!({"candidates":[{"finishReason":"MAX_TOKENS"}],"usageMetadata":usage});
        let complete = gemini::complete(&response, "resp_usage").unwrap();
        assert_eq!(
            complete["usage"]["completion_tokens"].as_u64(),
            expected,
            "{usage}"
        );
        let chunks = gemini::Stream::new("resp_usage").push(&response).unwrap();
        let streamed = chunks.iter().find_map(|chunk| chunk.get("usage")).unwrap();
        assert_eq!(streamed["completion_tokens"].as_u64(), expected, "{usage}");
    }
}

#[test]
fn gemini_stream_keeps_reasoning_separate_and_counts_thinking_tokens() {
    let mut converter = gemini::Stream::new("resp_123");
    let chunk = json!({"candidates":[{"content":{"parts":[{"text":"private thinking","thought":true},
        {"text":"visible"},{"functionCall":{"name":"read","args":{"path":"one"}},"thoughtSignature":"TEST_SIGNATURE"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":10,"thoughtsTokenCount":7,"cachedContentTokenCount":80,"totalTokenCount":117}});
    let chunks = converter.push(&chunk).unwrap();
    assert!(converter.finished());
    assert_eq!(
        chunks[0]["choices"][0]["delta"]["reasoning_content"],
        "private thinking"
    );
    assert_eq!(chunks[1]["choices"][0]["delta"]["content"], "visible");
    assert_eq!(
        chunks[2]["choices"][0]["delta"]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "TEST_SIGNATURE"
    );
    assert_eq!(chunks[3]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[4]["usage"]["completion_tokens"], 17);
    assert_eq!(
        chunks[4]["usage"]["prompt_tokens_details"]["cached_tokens"],
        80
    );
}
