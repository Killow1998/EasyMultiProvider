pub(crate) mod continuation;
pub(crate) mod diagnostics;
pub(crate) mod operation;
use emp_storage::diagnostics::Journal;
use serde_json::{Value, json};
pub(crate) mod http;
pub(crate) mod request;
pub(crate) fn retry_scheduled(
    journal: &Journal,
    request_id: &Value,
    attempt: usize,
    delay: std::time::Duration,
    error: &emp_router::RouterError,
    fallback: bool,
) {
    journal.event(
        "info",
        "model_retry_scheduled",
        &json!({
            "request_id":diagnostics::schema::id(request_id),
            "attempt":attempt + 1, "delay_ms":delay.as_millis() as u64,
            "status":error.status(), "error_class":error.error_class().as_str(),
            "protocol_fallback":fallback,
        }),
    );
}

/// An attempt is evidence of entering an executor, not proof of provider acceptance.
pub(crate) fn record_attempt(journal: &Journal, event: &Value, boundary: &'static str) {
    let safe = diagnostics::schema::route_record(event, journal);
    let mut fields = serde_json::Map::new();
    for key in [
        "request_id",
        "provider_id",
        "model_id",
        "upstream_model",
        "protocol",
        "dialect",
        "endpoint_fingerprint",
        "route_source",
    ] {
        fields.insert(key.into(), safe[key].clone());
    }
    fields.insert("attempt_boundary".into(), json!(boundary));
    journal.event("info", "model_attempt_started", &Value::Object(fields));
}
