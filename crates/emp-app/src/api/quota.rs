use crate::app::ServerState;
use crate::http::{request::Request, response::SECURITY_HEADERS};
use crate::server::http::{error::JsonReply, management::ManagementSession};
use crate::services::management_events::{Changes, Subscription};
use crate::util::system_now;
use std::{io::Write, net::TcpStream, sync::atomic::Ordering, time::Duration};

#[cfg(test)]
pub(crate) use crate::services::management_events::SUBSCRIBER_LIMIT as QUOTA_EVENT_SLOT_LIMIT;

pub(crate) const EVENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const QUOTA_EVENT_KEEP_ALIVE: Duration = Duration::from_secs(15);

pub(crate) fn subscriber_limit() -> JsonReply {
    JsonReply::error(503, "Too many quota subscribers", None).with_header("Retry-After", "15")
}

pub(crate) fn serve_quota_events(
    stream: &mut TcpStream,
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) {
    let (session, mut subscription) = match ManagementSession::require(request, state, now)
        .map_err(|error| error.reply())
        .and_then(|session| {
            state
                .backend
                .management_events
                .subscribe()
                .map(|subscription| (session, subscription))
                .ok_or_else(subscriber_limit)
        }) {
        Ok(authorized) => authorized,
        Err(reply) => {
            let _ = stream.write_all(&reply.wire_response());
            let _ = stream.flush();
            return;
        }
    };
    let mut head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nX-Accel-Buffering: no\r\n".to_vec();
    head.extend_from_slice(SECURITY_HEADERS);
    head.extend_from_slice(b"Connection: close\r\n\r\n");
    if stream.set_write_timeout(Some(EVENT_WRITE_TIMEOUT)).is_err()
        || stream.write_all(&head).is_err()
        || stream.flush().is_err()
    {
        return;
    }
    while let Some((frame, quit)) =
        state
            .backend
            .transport
            .runtime
            .block_on(next_frame(&mut subscription, &session, state))
    {
        if stream.write_all(&frame).is_err() || stream.flush().is_err() || quit {
            break;
        }
    }
}

pub(crate) async fn next_frame(
    subscription: &mut Subscription,
    session: &ManagementSession,
    state: &ServerState,
) -> Option<(Vec<u8>, bool)> {
    let changes = subscription.next(QUOTA_EVENT_KEEP_ALIVE).await?;
    if !state.sessions.contains(Some(&session.0), system_now()) {
        return None;
    }
    if changes.quit {
        return Some((b"event: emp-exiting\ndata: {}\n\n".to_vec(), true));
    }
    if state.shutdown.load(Ordering::Acquire) {
        return None;
    }
    Some((change_frame(changes, state), false))
}

fn change_frame(changes: Changes, state: &ServerState) -> Vec<u8> {
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
    frame
}
