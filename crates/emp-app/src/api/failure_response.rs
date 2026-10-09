//! Preserve the distinct HTTP, SSE and WebSocket error contracts.

use crate::http::response::response;
use crate::http::response::status_text;
use crate::services::error_origin;
use crate::services::external::ExternalRequestError;
use crate::services::failure_feedback;
use emp_core::ResolvedRoute;
use emp_core::RouteResolutionError;
use emp_router::RouterError;
use emp_router::RouterErrorKind;
use emp_transport::FailureClass;
use emp_transport::normalize_error_class;
use emp_transport::public_failure_message;
use serde_json::Value;

mod stream;
pub(crate) use stream::{safe_failure_reason, stream_error_code};
pub(crate) use stream::{
    stream_failure_value, stream_failure_value_for_route, websocket_router_error,
    websocket_router_error_for_route,
};

pub(crate) fn emp_websocket_error(status: u16, code: &str, message: &str) -> Value {
    serde_json::json!({"type":"error", "status":status,
        "error":{"code":code,"origin":"emp","message":error_origin::message("emp", message)}})
}

pub(crate) fn external_complete_error(error: ExternalRequestError) -> Vec<u8> {
    external_http_error(error, |error, route| {
        router_error_response_for_route(error, &route)
    })
}

pub(crate) fn external_open_error(error: ExternalRequestError) -> Vec<u8> {
    external_http_error(error, |error, route| {
        pre_output_router_error_response_for_route(&error, &route)
    })
}

fn external_http_error(
    error: ExternalRequestError,
    router_response: impl FnOnce(RouterError, Box<ResolvedRoute>) -> Vec<u8>,
) -> Vec<u8> {
    match error {
        ExternalRequestError::Router(error, route) => router_response(error, route),
        ExternalRequestError::Route(error) => route_resolution_response(error),
        ExternalRequestError::Disconnected => Vec::new(),
        ExternalRequestError::Unsupported => crate::http::response::json_error_response(
            503,
            status_text(503),
            "provider protocol is unsupported",
            Some("router_error"),
            &[],
        ),
    }
}

pub(crate) fn external_websocket_error(error: ExternalRequestError) -> Value {
    match error {
        ExternalRequestError::Router(error, route) => {
            websocket_router_error_for_route(&error, &route)
        }
        ExternalRequestError::Route(error) => {
            emp_websocket_error(error.status(), "router_error", &error.to_string())
        }
        ExternalRequestError::Unsupported => {
            emp_websocket_error(503, "router_error", "provider protocol is unsupported")
        }
        ExternalRequestError::Disconnected => serde_json::json!({
            "type":"error", "status":499,
            "error":{"code":"client_disconnected","origin":"client","message":"[Client] client disconnected"}
        }),
    }
}

pub(crate) fn route_resolution_response(error: RouteResolutionError) -> Vec<u8> {
    let error_class = if error.status() >= 500 {
        "upstream_5xx"
    } else {
        "router_error"
    };
    http_error(
        error.status(),
        serde_json::json!({
            "code":error_class, "type":error_class, "origin":"emp",
            "message":error_origin::message("emp", &error.to_string()),
        }),
        None,
    )
}

pub(crate) fn request_router_error_response(status: u16, message: &str) -> Vec<u8> {
    http_error(
        status,
        serde_json::json!({
            "code":"router_error", "type":"router_error", "origin":"emp",
            "message":error_origin::message("emp", message),
        }),
        None,
    )
}

fn routed_message(error: &RouterError, route: &ResolvedRoute, output_started: bool) -> String {
    let detail = (error.kind() != RouterErrorKind::Upstream
        && error.kind() != RouterErrorKind::Transport)
        .then(|| error.to_string());
    failure_feedback::message(
        route,
        error.error_class(),
        error.failure_reason(),
        error.status(),
        output_started,
        detail.as_deref(),
    )
}

pub(crate) fn router_error_response(error: RouterError) -> Vec<u8> {
    router_error_response_with_route(error, None)
}

pub(crate) fn router_error_response_for_route(
    error: RouterError,
    route: &ResolvedRoute,
) -> Vec<u8> {
    router_error_response_with_route(error, Some(route))
}

fn router_error_response_with_route(error: RouterError, route: Option<&ResolvedRoute>) -> Vec<u8> {
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
        "message": route.map_or_else(
            || if error.kind() == RouterErrorKind::Upstream {
                public_failure_message(error.error_class(), error.failure_reason(), error.status())
            } else {
                error.to_string()
            },
            |route| routed_message(&error, route, false),
        ),
    });
    error_origin::annotate(&mut detail, error_origin::router(&error));
    if let Some(reason) = failure_reason {
        detail["failure_reason"] = Value::String(reason);
    }
    http_error(error.status(), detail, error.retry_after_seconds())
}

pub(crate) fn pre_output_failure_response(event: &Value, route: &ResolvedRoute) -> Option<Vec<u8>> {
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
        "message": failure_feedback::message(
            route, error_class, failure_reason, status, false, None,
        ),
        "param": Value::Null,
    });
    error_origin::annotate(&mut detail, "upstream");
    if let Some(reason) = failure_reason {
        detail["failure_reason"] = Value::String(reason.to_owned());
    }
    let retry_after = error
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("retry-after"))
        })
        .and_then(|(_, value)| emp_router::parse_retry_after(value.as_str()))
        .or_else(|| error.get("retry_after_seconds").and_then(Value::as_u64));
    Some(http_error(status, detail, retry_after))
}

pub(crate) fn pre_output_router_error_response_for_route(
    error: &RouterError,
    route: &ResolvedRoute,
) -> Vec<u8> {
    pre_output_router_error_response_with_route(error, Some(route))
}

fn pre_output_router_error_response_with_route(
    error: &RouterError,
    route: Option<&ResolvedRoute>,
) -> Vec<u8> {
    let error_class = error.error_class();
    let mut detail = serde_json::json!({
        "type": error_class.as_str(),
        "code": stream_error_code(error_class),
        "message": route.map_or_else(
            || public_failure_message(error_class, error.failure_reason(), error.status()),
            |route| routed_message(error, route, false),
        ),
        "param": Value::Null,
    });
    error_origin::annotate(&mut detail, error_origin::router(error));
    if let Some(reason) = error.failure_reason()
        && error_class != FailureClass::StreamIncomplete
    {
        detail["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    http_error(error.status(), detail, error.retry_after_seconds())
}

// HTTP responses keep Retry-After in the actual header. The compatibility field
// remains available to older clients; no other upstream headers are exposed.
fn http_error(status: u16, mut detail: Value, retry_after: Option<u64>) -> Vec<u8> {
    if let Some(delay) = retry_after {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&serde_json::json!({"error":detail}))
        .expect("error response is JSON serializable");
    let retry = retry_after.map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &body,
        &headers,
    )
}
