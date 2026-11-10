//! Owner-scoped lifetime aggregates streamed to service details.
use super::*;
use crate::services::events::sse_frame;
use std::io::Write;

pub(crate) fn stream(
    stream: &mut TcpStream,
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) -> std::io::Result<()> {
    if !same_origin(request, state.port) {
        return stream.write_all(&cross_origin_response("management session is required"));
    }
    if !state
        .sessions
        .contains(request.session_token().as_deref(), now)
    {
        return stream.write_all(&unauthorized_response());
    }
    let accounts = query_values(request.target, "account_id");
    let owners = query_values(request.target, "owner");
    let owner = match (accounts.as_slice(), owners.as_slice()) {
        ([account], []) => match quota_owner_key(state, account) {
            Ok(owner) => owner,
            Err(_) => {
                return stream.write_all(&json_error_response(
                    404,
                    status_text(404),
                    "Account usage is unavailable",
                    None,
                    &[],
                ));
            }
        },
        ([], [owner])
            if !owner.is_empty() && owner.len() <= 256 && !owner.chars().any(char::is_control) =>
        {
            owner.clone()
        }
        _ => return stream.write_all(&invalid()),
    };
    let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream)?;
    let writer = stream.try_clone()?;
    crate::api::streaming::write_stream_head(stream)?;
    stream.flush()?;
    let result = state.backend.usage.ledger.query_lifetime_with_progress(
        &owner,
        now,
        move |completed, total| {
            let mut writer = &writer;
            writer.write_all(&sse_frame(
                "progress",
                &json!({"completed":completed,"total":total}),
            )?)?;
            writer.flush()
        },
        Some(monitor.cancellation()),
    );
    let frame = match result {
        Ok(payload) => sse_frame("result", &payload)?,
        Err(_) => sse_frame(
            "error",
            &json!({"error":{"message":"Usage history is unavailable"}}),
        )?,
    };
    stream.write_all(&frame)?;
    stream.flush()
}
