use emp_transport::{SseFrame, SseJsonParser, TransportErrorReason, sse_json_events};
use serde_json::json;

#[test]
fn chunk_boundaries_crlf_multiline_and_done_match_sse_semantics() {
    let chunks = [
        b": keepalive\r\ndata: {\"type\":\r\n".as_slice(),
        b"data: \"first\"}\r\n\r\ndata: [DONE]\n\n".as_slice(),
        b"event: ignored\ndata: {\"type\":\"last\"}\n\n".as_slice(),
    ];
    assert_eq!(
        sse_json_events(chunks).expect("valid SSE"),
        vec![
            json!({"type": "first"}).as_object().unwrap().clone(),
            json!({"type": "last"}).as_object().unwrap().clone(),
        ]
    );
}

#[test]
fn trailing_event_without_blank_line_is_flushed() {
    let mut parser = SseJsonParser::new();
    assert!(
        parser
            .push(b"data: {\"type\":\"tail\"}")
            .expect("partial event")
            .is_empty()
    );
    assert_eq!(
        parser.finish().expect("flush event"),
        vec![json!({"type": "tail"}).as_object().unwrap().clone()]
    );
}

#[test]
fn ordered_frames_expose_done_without_changing_json_compatibility() {
    let wire = b"data: {\"type\":\"before\"}\n\ndata: [DONE]\n\ndata: {\"type\":\"after\"}\n\n";
    let mut parser = SseJsonParser::new();
    let frames = parser.push_frames(wire).expect("ordered SSE frames");
    assert_eq!(
        frames,
        vec![
            SseFrame::Json(json!({"type": "before"}).as_object().unwrap().clone()),
            SseFrame::Done,
            SseFrame::Json(json!({"type": "after"}).as_object().unwrap().clone()),
        ]
    );
}

#[test]
fn aggregate_limit_and_failure_reasons_are_content_free() {
    let mut parser = SseJsonParser::with_limit(32).expect("test parser");
    let error = parser
        .push(b"data: 12345678901234567890\ndata: 12345678901234567890\n\n")
        .expect_err("aggregate event exceeds limit");
    assert_eq!(error.reason(), TransportErrorReason::SseEventTooLarge);
    assert_eq!(error.failure_reason(), "sse_event_too_large");

    for (wire, reason) in [
        (
            b"data: private invalid content\n\n".as_slice(),
            TransportErrorReason::SseInvalidJson,
        ),
        (
            b"data: []\n\n".as_slice(),
            TransportErrorReason::SseNonObject,
        ),
    ] {
        let error = sse_json_events([wire]).expect_err("invalid SSE event");
        assert_eq!(error.reason(), reason);
        assert!(!error.to_string().contains("private"));
    }
}

#[test]
fn many_small_events_in_one_chunk_do_not_share_a_limit() {
    let wire = b"data: {\"type\":\"ping\"}\n\n".repeat(20);
    let mut parser = SseJsonParser::with_limit(32).expect("test parser");
    let events = parser.push(&wire).expect("small events");
    assert_eq!(events.len(), 20);
    assert!(parser.finish().expect("empty finish").is_empty());
}

#[test]
fn byte_at_a_time_feed_matches_single_chunk_parse() {
    let wire = b": ping\r\ndata: {\"type\":\r\ndata: \"split\"}\r\n\r\ndata: [DONE]\n\ndata: {\"type\":\"tail\"}\n\n";
    let mut whole = SseJsonParser::new();
    let expected = whole.push_frames(wire).expect("whole");
    let mut split = SseJsonParser::new();
    let mut actual = Vec::new();
    for byte in wire {
        actual.extend(
            split
                .push_frames(std::slice::from_ref(byte))
                .expect("byte chunk"),
        );
    }
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 3);
}

#[test]
fn long_unterminated_line_across_chunks_still_hits_limit() {
    let mut parser = SseJsonParser::with_limit(64).expect("limit");
    for _ in 0..8 {
        parser.push_frames(b"data: xx").expect("under limit");
    }
}

#[test]
fn multibyte_utf8_split_across_chunks_decodes_exactly_once() {
    let wire = "data: {\"text\":\"思考\"}\n\n".as_bytes();
    let split = wire.iter().position(|byte| *byte >= 0x80).unwrap() + 1;
    let mut parser = SseJsonParser::new();
    assert!(parser.push(&wire[..split]).unwrap().is_empty());
    let mut events = parser.push(&wire[split..]).unwrap();
    events.extend(parser.finish().unwrap());
    assert_eq!(
        events,
        vec![json!({"text": "思考"}).as_object().unwrap().clone()]
    );
}
