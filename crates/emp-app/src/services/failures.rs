//! Services failures.

use crate::http::response::response;
use crate::http::response::status_text;
use emp_core::ResolvedRoute;
use emp_core::RouteResolutionError;
use emp_router::RouterError;
use emp_router::RouterErrorKind;
use emp_transport::FailureClass;
use emp_transport::FailurePhase;
use emp_transport::UpstreamFailure;
use emp_transport::external_http_retry_allowed;
use emp_transport::normalize_error_class;
use emp_transport::public_failure_message;
use serde_json::Value;
use std::time::Duration;

pub(crate) fn route_resolution_response(error: RouteResolutionError) -> Vec<u8> {
    let error_class = if error.status() >= 500 {
        "upstream_5xx"
    } else {
        "router_error"
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": error_class,
            "type": error_class,
            "message": error.to_string(),
        }
    }))
    .expect("route resolution response is JSON serializable");
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &[],
    )
}

pub(crate) fn request_router_error_response(status: u16, message: &str) -> Vec<u8> {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": "router_error",
            "type": "router_error",
            "message": message,
        }
    }))
    .expect("request router response is JSON serializable");
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &body,
        &[],
    )
}

pub(crate) fn router_error_response(error: RouterError) -> Vec<u8> {
    let failure_reason = error.failure_reason().map(str::to_owned);
    let error_class = error.error_class().as_str();
    let code = if error_class == "rate_limit" {
        "rate_limit_exceeded".to_owned()
    } else {
        failure_reason
            .clone()
            .unwrap_or_else(|| error_class.to_owned())
    };
    let mut detail = serde_json::json!({
        "code": code,
        "type": error_class,
        "message": if error.kind() == RouterErrorKind::Upstream {
            public_failure_message(error.error_class(), error.failure_reason(), error.status())
        } else {
            error.to_string()
        },
    });
    if let Some(reason) = failure_reason {
        detail["failure_reason"] = Value::String(reason);
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail}))
        .expect("router response is JSON serializable");
    let retry = error.retry_after_seconds().map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &headers,
    )
}

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
    let error_class = error.error_class();
    let mut detail = serde_json::json!({
        "code": stream_error_code(error_class),
        "message": format!(
            "HTTP {}: {}",
            error.status(),
            public_failure_message(error_class, error.failure_reason(), error.status())
        ),
        "status": error.status(),
        "error_class": error_class.as_str(),
    });
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
    }
    serde_json::json!({
        "type": "response.failed",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "failed",
            "error": detail,
        }
    })
}

pub(crate) fn pre_output_failure_response(event: &Value) -> Option<Vec<u8>> {
    if event.get("type").and_then(Value::as_str) != Some("response.failed") {
        return None;
    }
    let error = event.get("response")?.get("error")?.as_object()?;
    let status = error
        .get("status")?
        .as_u64()
        .and_then(|status| u16::try_from(status).ok())?;
    if !(400..=599).contains(&status) {
        return None;
    }
    let error_class_name = error
        .get("error_class")
        .and_then(Value::as_str)
        .unwrap_or("upstream_error");
    let error_class = normalize_error_class(Some(error_class_name), FailureClass::StreamError);
    let failure_reason = error.get("failure_reason").and_then(Value::as_str);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_else(|| stream_error_code(error_class));
    let mut detail = serde_json::json!({
        "type": error_class_name,
        "code": code,
        "message": public_failure_message(error_class, failure_reason, status),
        "param": Value::Null,
    });
    if let Some(reason) = failure_reason {
        detail["failure_reason"] = Value::String(reason.to_owned());
    }
    let retry_after = error.get("retry_after_seconds").and_then(Value::as_u64);
    if let Some(delay) = retry_after {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail})).ok()?;
    let retry = retry_after.map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    Some(response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &body,
        &headers,
    ))
}

pub(crate) fn pre_output_router_error_response(error: &RouterError) -> Vec<u8> {
    let error_class = error.error_class();
    let mut detail = serde_json::json!({
        "type": error_class.as_str(),
        "code": stream_error_code(error_class),
        "message": public_failure_message(error_class, error.failure_reason(), error.status()),
        "param": Value::Null,
    });
    if let Some(reason) = error.failure_reason()
        && error_class != FailureClass::StreamIncomplete
    {
        detail["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error": detail}))
        .expect("stream error response is JSON serializable");
    let retry = error.retry_after_seconds().map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &headers,
    )
}

pub(crate) fn external_retry_delay(
    error: &RouterError,
    attempt: usize,
    route: &ResolvedRoute,
) -> Option<Duration> {
    let failure = UpstreamFailure {
        error_class: error.error_class(),
        status: error.status(),
        phase: FailurePhase::TerminalValidation,
        terminal_event: false,
        failure_reason: error.failure_reason().map(str::to_owned),
        retry_after_seconds: error.retry_after_seconds(),
    };
    let free_route = route
        .upstream_model
        .trim()
        .to_ascii_lowercase()
        .ends_with(":free");
    external_http_retry_allowed(&failure, attempt, false, false, free_route)
        .then(|| Duration::from_secs(failure.retry_after_seconds.unwrap_or(1)))
}
