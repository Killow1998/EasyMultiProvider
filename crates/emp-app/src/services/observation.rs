//! Usage, diagnostics and activity reporting; request outcomes own routing feedback.
pub(crate) mod continuation;
pub(crate) mod operation;
use crate::app::ServerState;
use crate::util::system_now;
use emp_core::ResolvedRoute;
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub(crate) mod http;
pub(crate) mod request;
pub(crate) fn retry_scheduled(
    state: &ServerState,
    request_id: &Value,
    attempt: usize,
    delay: std::time::Duration,
    error: &emp_router::RouterError,
    fallback: bool,
) {
    state.backend.diagnostics.journal.event(
        "info",
        "model_retry_scheduled",
        &json!({
            "request_id":emp_state::diagnostics::schema::id(request_id),
            "attempt":attempt + 1, "delay_ms":delay.as_millis() as u64,
            "status":error.status(), "error_class":error.error_class().as_str(),
            "protocol_fallback":fallback,
        }),
    );
}

/// An attempt is evidence of entering an executor, not proof of provider acceptance.
pub(crate) fn record_attempt(state: &ServerState, event: &Value, boundary: &'static str) {
    let safe =
        emp_state::diagnostics::schema::route_record(event, &state.backend.diagnostics.journal);
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
    state.backend.diagnostics.journal.event(
        "info",
        "model_attempt_started",
        &Value::Object(fields),
    );
}

pub(crate) fn request_cancelled(
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
) {
    record_cancellation(state, route, incoming, "client_disconnect", None);
}

pub(crate) fn request_interrupted(
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
) {
    record_cancellation(
        state,
        route,
        incoming,
        "client_cancelled",
        Some("interrupted"),
    );
}

fn record_cancellation(
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    error_class: &str,
    reason: Option<&str>,
) {
    let event = json!({"request_id":request::request_id(incoming),
        "error_class":error_class,"error_origin":"client","failure_reason":reason,"success":false});
    state
        .backend
        .diagnostics
        .journal
        .event("info", "model_request_cancelled", &event);
    state.backend.activity.observe_request(
        &event,
        crate::services::activity::ActivityIdentity::from_route(route).as_ref(),
        true,
    );
}

pub(crate) fn record_completion(state: &ServerState, event: &Value) {
    state.backend.usage.ledger.record(event, system_now());
    state.backend.diagnostics.record(event);
}
