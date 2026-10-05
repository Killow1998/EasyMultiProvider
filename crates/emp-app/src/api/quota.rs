//! Api quota.

use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::percent_decode;
use crate::http::request::read_json_body;
use crate::http::response::SECURITY_HEADERS;
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
use crate::http::response::not_found_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use crate::util::system_now;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(test)]
pub(crate) use crate::services::management_events::SUBSCRIBER_LIMIT as QUOTA_EVENT_SLOT_LIMIT;

const QUOTA_EVENT_KEEP_ALIVE: Duration = Duration::from_secs(15);

pub(crate) fn serve_quota_events(
    stream: &mut TcpStream,
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) {
    if !same_origin(request, state.port) {
        let _ = stream.write_all(&cross_origin_response("management session is required"));
        let _ = stream.flush();
        return;
    }
    let session = request.session_token();
    if !state.sessions.contains(session.as_deref(), now) {
        let _ = stream.write_all(&unauthorized_response());
        let _ = stream.flush();
        return;
    }
    let Some(mut subscription) = state.backend.management_events.subscribe() else {
        let response = json_error_response(
            503,
            status_text(503),
            "Too many quota subscribers",
            None,
            &[("Retry-After", "15")],
        );
        let _ = stream.write_all(&response);
        let _ = stream.flush();
        return;
    };
    let mut head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\n".to_vec();
    head.extend_from_slice(SECURITY_HEADERS);
    head.extend_from_slice(b"Connection: close\r\n\r\n");
    if stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .is_err()
        || stream.write_all(&head).is_err()
        || stream.flush().is_err()
    {
        return;
    }
    while !state.shutdown.load(Ordering::Acquire) {
        let Some(changes) = subscription.next(QUOTA_EVENT_KEEP_ALIVE) else {
            break;
        };
        if state.shutdown.load(Ordering::Acquire)
            || !state.sessions.contains(session.as_deref(), system_now())
        {
            break;
        }
        let mut frame = Vec::new();
        if changes.quota {
            frame.extend_from_slice(b"event: quota-updated\ndata: {}\n\n");
        }
        if changes.integration {
            frame.extend_from_slice(b"event: integration-updated\ndata: {}\n\n");
        }
        if changes.activity {
            let snapshot = state
                .backend
                .activity
                .snapshot(system_now().max(0.0) as u64);
            if let Ok(activity_frame) =
                crate::services::events::sse_frame("activity-updated", &snapshot)
            {
                frame.extend_from_slice(&activity_frame);
            }
        }
        if changes.usage {
            frame.extend_from_slice(b"event: usage-updated\ndata: {}\n\n");
        }
        if frame.is_empty() {
            frame.extend_from_slice(b": keep-alive\n\n");
        }
        if stream.write_all(&frame).is_err() || stream.flush().is_err() {
            break;
        }
    }
}

pub(crate) fn management_quota_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    let supplied_session = request.session_token();
    if !state.sessions.contains(supplied_session.as_deref(), now) {
        return unauthorized_response();
    }
    let path = request.raw_path();
    let reset = path.ends_with("/quota-reset");
    let suffix = if reset { "/quota-reset" } else { "/quota" };
    let Some(raw_account) = path
        .strip_prefix("/api/accounts/")
        .and_then(|rest| rest.strip_suffix(suffix))
        .map(|raw| raw.trim_end_matches('/'))
        .filter(|raw| !raw.is_empty())
    else {
        return not_found_response();
    };
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let account = percent_decode(raw_account, false);
    use crate::services::quota::commands::{self, CommandError};
    match commands::execute(request.observation_id, state, &account, reset, &body) {
        Ok(payload) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&payload).expect("quota command result"),
            &[],
        ),
        Err(CommandError::Internal) => {
            json_error_response(500, status_text(500), "internal server error", None, &[])
        }
        Err(CommandError::UnknownAccount(account)) => json_error_response(
            503,
            status_text(503),
            &format!("unknown account: {account}"),
            Some("quota_error"),
            &[],
        ),
        Err(CommandError::InvalidCredit) => json_error_response(
            400,
            status_text(400),
            "reset credit id is invalid",
            Some("quota_reset_invalid_request"),
            &[],
        ),
        Err(CommandError::Reset(error)) => {
            let status = if error.code() == "quota_reset_invalid_request" {
                400
            } else {
                503
            };
            json_error_response(
                status,
                status_text(status),
                &error.to_string(),
                Some(error.code()),
                &[],
            )
        }
        Err(CommandError::Refresh(error)) => json_error_response(
            503,
            status_text(503),
            &error.to_string(),
            Some(error.code()),
            &[],
        ),
    }
}
