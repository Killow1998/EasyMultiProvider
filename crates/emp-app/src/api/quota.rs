use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::response::SECURITY_HEADERS;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
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
    loop {
        let Some(changes) = subscription.next(QUOTA_EVENT_KEEP_ALIVE) else {
            break;
        };
        if !state.sessions.contains(session.as_deref(), system_now()) {
            break;
        }
        if changes.quit {
            let _ = stream.write_all(b"event: emp-exiting\ndata: {}\n\n");
            let _ = stream.flush();
            break;
        }
        if state.shutdown.load(Ordering::Acquire) {
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
