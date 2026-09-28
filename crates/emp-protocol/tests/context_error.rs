//! Context-length classification: users must see "context exceeded" surfaced
//! from structured provider errors, never from generic or WAF responses.

use emp_protocol::context_error::is_explicit_context_error;
use serde_json::json;

fn classify(status: u16, content_type: &str, value: &serde_json::Value) -> bool {
    is_explicit_context_error(status, content_type, value.to_string().as_bytes())
}

#[test]
fn structured_context_markers_classify_but_generic_errors_do_not() {
    assert!(classify(
        400,
        "application/json",
        &json!({"error": {"message": "This model's maximum context length is 128000 tokens"}})
    ));
    assert!(classify(
        413,
        "application/json",
        &json!({"error": {"code": "context_length_exceeded"}})
    ));

    // No provider-specific evidence, no claim.
    assert!(!classify(
        400,
        "application/json",
        &json!({"error": {"message": "invalid api key"}})
    ));
    // HTML error pages from gateways never count as evidence.
    let html: &serde_json::Value = &serde_json::Value::String(
        "<html><body>maximum context length blocked by WAF</body></html>".to_owned(),
    );
    assert!(!classify(400, "text/html", html));
    // Success without structured evidence is not a context error either.
    assert!(!classify(200, "application/json", &json!({"ok": true})));
}
