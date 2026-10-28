//! Native WebSocket wire errors; routing and retry decisions remain with the turn.
use crate::api::failure_response::{safe_failure_reason, stream_error_code};
use crate::services::observation::continuation::ContinuationReason;
use emp_core::ResolvedRoute;
use emp_transport::{FailureClass, public_failure_message};
use serde_json::Value;

pub(in crate::api::websocket) fn native_stream_error_value(
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<&str>,
    response_id: &str,
) -> Value {
    let mut error = serde_json::json!({
        "code":stream_error_code(error_class, failure_reason),
        "message":format!("HTTP {status}: {}",public_failure_message(error_class,failure_reason,status)),
        "status":status,"error_class":error_class.as_str()
    });
    if let Some(reason) = failure_reason {
        error["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    crate::services::error_origin::annotate(
        &mut error,
        if error_class == FailureClass::ContextLengthExceeded {
            "emp"
        } else {
            "transport"
        },
    );
    serde_json::json!({"type":"response.failed","response":{"id":response_id,"object":"response","status":"failed","error":error}})
}

pub(super) fn native_stream_error_value_for_route(
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<&str>,
    response_id: &str,
    route: &ResolvedRoute,
    output_started: bool,
) -> Value {
    let mut event = native_stream_error_value(status, error_class, failure_reason, response_id);
    let origin = event["response"]["error"]["origin"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    event["response"]["error"]["message"] = Value::String(crate::services::error_origin::message(
        &origin,
        &format!(
            "HTTP {status}: {}",
            crate::services::failure_feedback::message(
                route,
                error_class,
                failure_reason,
                status,
                output_started,
                None,
            )
        ),
    ));
    event
}

pub(in crate::api::websocket) fn previous_response_error(reason: ContinuationReason) -> Value {
    let mut event = crate::api::failure_response::emp_websocket_error(
        400,
        "previous_response_not_found",
        "Previous response was not found. Retry with full history.",
    );
    event["error"]["failure_reason"] =
        serde_json::to_value(reason).expect("bounded continuation reason");
    event
}
