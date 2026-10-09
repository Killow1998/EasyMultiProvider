use super::*;

#[test]
fn text_with_many_parts_keeps_the_original_transcript() {
    let body = json!({"input":[{
        "type":"message","role":"user","id":"x".repeat(64 * 1024),
        "content":vec![json!({"type":"input_text","text":"hello"}); 192]
    }]});
    let prepared = prepare(&body).unwrap();
    assert!(matches!(prepared.input_format, InputFormat::Text));
    assert_eq!(prepared.stdin, projection::transcript(&body).unwrap());
    assert!(prepared.stdin.len() < 100 * 1024);
}

#[test]
fn media_and_tool_output_stop_expanding_metadata_at_the_budget() {
    for tool_output in [false, true] {
        let mut parts = vec![json!({"type":"input_text","text":"hello"}); 192];
        // This tail is deliberately invalid. Reaching it would mean the
        // converter had already accumulated over 10 MiB of repeated metadata.
        parts.push(json!({"type":"input_audio","input_audio":{"data":"AAAA"}}));
        let item = if tool_output {
            json!({"type":"function_call_output","call_id":"quoted\"".repeat(8192),"output":parts})
        } else {
            json!({"type":"message","role":"user","id":"x".repeat(64 * 1024),"content":parts})
        };
        let body = json!({"input":[item]});
        assert!(serde_json::to_vec(&body).unwrap().len() < 100 * 1024);
        let error = if tool_output {
            context_payload(&body).err()
        } else {
            prepare(&body).err()
        };
        assert_eq!(error, Some("claude_cli_input_too_large"));
    }
}
