//! Usage, diagnostics and activity reporting; request outcomes own routing feedback.
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
    state.backend.activity.request_retry(
        request_id,
        if fallback {
            "protocol_rejection"
        } else {
            error.error_class().as_str()
        },
        error.status(),
    );
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

pub(crate) fn request_tokens_per_second(output_tokens: &Value, duration_ms: &Value) -> Option<f64> {
    let tokens = output_tokens
        .as_u64()
        .filter(|tokens| (1..=10_000_000).contains(tokens))?;
    let duration = duration_ms
        .as_f64()
        .filter(|duration| duration.is_finite() && (100.0..=86_400_000.0).contains(duration))?;
    let rate = tokens as f64 * 1000.0 / duration;
    (rate > 0.0 && rate <= 1_000_000.0).then(|| (rate * 100.0).round_ties_even() / 100.0)
}

pub(crate) fn execution_attempt(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) {
    let event = json!({
        "request_id":incoming.iter().find(|(name, _)| name.eq_ignore_ascii_case("x-emp-request-id")).map(|(_, value)| value),
        "dispatch_started":true, "client_model":body["model"], "upstream_model":route.upstream_model,
        "provider_id":route.provider_id,"model_id":route.requested_model, "dialect":route.dialect,
        "endpoint_fingerprint":route.endpoint_fingerprint, "route_source":route.source,
        "resolved_protocol":route.protocol,
        "transport":if body["stream"]==true { "sse" } else { "http" },
    });
    record_attempt(
        state,
        &event,
        if crate::services::claude_cli::selected(route) {
            "cli_execution"
        } else {
            "router_dispatch"
        },
    );
    state.backend.activity.observe_request(
        &event,
        crate::services::activity::ActivityIdentity::from_route(route).as_ref(),
        false,
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
    let event = json!({"request_id":request::request_id(incoming),
        "error_class":"client_disconnect","success":false});
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
