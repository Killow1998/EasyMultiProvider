use crate::app::ServerState;
use crate::server::http::{error::JsonReply, management::ManagementSession};
use crate::services::management_events::{Changes, Subscription};
use crate::util::system_now;
use std::{sync::atomic::Ordering, time::Duration};

#[cfg(test)]
pub(crate) use crate::services::management_events::SUBSCRIBER_LIMIT as QUOTA_EVENT_SLOT_LIMIT;

const QUOTA_EVENT_KEEP_ALIVE: Duration = Duration::from_secs(15);

pub(crate) fn subscriber_limit() -> JsonReply {
    JsonReply::error(503, "Too many quota subscribers", None).with_header("Retry-After", "15")
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
    if state.lifecycle.shutdown.load(Ordering::Acquire) {
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
