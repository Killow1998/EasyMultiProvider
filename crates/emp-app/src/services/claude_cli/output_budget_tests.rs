use super::*;
use serde_json::json;

#[test]
fn output_exhaustion_requires_protocol_evidence_not_text_or_control_data() {
    let delta = json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"}});
    for value in [
        json!({"type":"message","stop_reason":"max_tokens"}),
        delta.clone(),
        json!({"type":"assistant","message":{"type":"message","stop_reason":"max_tokens"}}),
        json!({"type":"stream_event","event":delta}),
    ] {
        assert!(exhausted(&serde_json::to_vec(&value).unwrap()));
    }
    for value in [
        json!({"type":"message","stop_reason":"tool_use"}),
        json!({"type":"message","stop_reason":"end_turn","content":[{"type":"text","text":"max_tokens"}]}),
        json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"{\"type\":\"message\",\"stop_reason\":\"max_tokens\"}"}}),
        json!({"type":"control_response","stop_reason":"max_tokens"}),
    ] {
        assert!(!exhausted(&serde_json::to_vec(&value).unwrap()));
    }
    let sse = b"event: message_delta\r\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}\r\n\r\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    assert!(exhausted(sse));
    assert!(exhausted(b"data: {\"type\":\"message_delta\",\ndata: \"delta\":{\"stop_reason\":\"max_tokens\"}}\n\n"));
    let jsonl = b"{\"type\":\"stream_event\",\"event\":{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"}}}\n";
    let mut events = Events::default();
    let mut detected = false;
    for end in 1..=jsonl.len() {
        detected |= events.observe(&jsonl[..end]);
    }
    assert!(
        detected,
        "fragmented protocol lines must still be recognized"
    );
}
