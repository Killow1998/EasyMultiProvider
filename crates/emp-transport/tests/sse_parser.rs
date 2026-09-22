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
