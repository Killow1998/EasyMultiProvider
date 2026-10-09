//! SSE and WebSocket share the error detail, not an intermediate wire envelope.

use super::routed_message;
use crate::services::error_origin;
use emp_core::ResolvedRoute;
use emp_router::{RouterError, RouterErrorKind};
use emp_transport::{FailureClass, public_failure_message};
use serde_json::Value;

pub(crate) fn stream_error_code(error_class: FailureClass) -> &'static str {
    match error_class {
        FailureClass::ContextLengthExceeded => "context_length_exceeded",
        FailureClass::PaymentRequired => "payment_required",
        FailureClass::RateLimit => "rate_limit_exceeded",
        _ => "upstream_error",
    }
}

pub(crate) fn safe_failure_reason(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

pub(crate) fn stream_failure_value(error: &RouterError, response_id: &str) -> Value {
    stream_failure_value_with_route(error, response_id, None, false)
}

pub(crate) fn stream_failure_value_for_route(
    error: &RouterError,
    response_id: &str,
    route: &ResolvedRoute,
    output_started: bool,
) -> Value {
    stream_failure_value_with_route(error, response_id, Some(route), output_started)
}

fn stream_failure_value_with_route(
    error: &RouterError,
    response_id: &str,
    route: Option<&ResolvedRoute>,
    output_started: bool,
) -> Value {
    let detail = error_detail(error, route, output_started);
    serde_json::json!({
        "type": "response.failed",
        "response": {"id": response_id, "object": "response", "status": "failed", "error": detail}
    })
}

fn error_detail(error: &RouterError, route: Option<&ResolvedRoute>, output_started: bool) -> Value {
    let error_class = error.error_class();
    let mut detail = serde_json::json!({
        "code": stream_error_code(error_class),
        "message": format!(
            "HTTP {}: {}",
            error.status(),
            route.map_or_else(
                || public_failure_message(error_class, error.failure_reason(), error.status()),
                |route| routed_message(error, route, output_started),
            )
        ),
        "status": error.status(),
        "error_class": error_class.as_str(),
    });
    error_origin::annotate(&mut detail, error_origin::router(error));
    if let Some(reason) = error.failure_reason() {
        let reason = safe_failure_reason(reason);
        if !reason.is_empty() {
            detail["failure_reason"] = Value::String(reason);
        }
    }
    if error.kind() == RouterErrorKind::Transport {
        detail["transport_failure"] = Value::Bool(true);
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
        if error_class == FailureClass::RateLimit {
            detail["message"] = Value::String(format!(
                "{} Please try again in {delay}s.",
                detail["message"].as_str().unwrap_or_default()
            ));
        }
        // Codex reads streamed and wrapped WebSocket retry advice here.
        detail["headers"] = serde_json::json!({"Retry-After": delay.to_string()});
    }
    detail
}

pub(crate) fn websocket_router_error(error: &RouterError) -> Value {
    let detail = error_detail(error, None, false);
    serde_json::json!({
        "type":"error", "status":error.status(),
        "error":detail
    })
}

pub(crate) fn websocket_router_error_for_route(
    error: &RouterError,
    route: &ResolvedRoute,
) -> Value {
    let detail = error_detail(error, Some(route), false);
    serde_json::json!({
        "type":"error", "status":error.status(),
        "error":detail
    })
}
