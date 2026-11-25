//! Standalone Codex image tools use their own JSON endpoints, outside Responses.
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::services::native::images;
use serde_json::Value;

pub(crate) fn credit_status(state: &ServerState) -> HttpReply {
    match images::credit_status(state) {
        Ok(status) => HttpReply::new(200, status.to_string().into_bytes()),
        Err(error) => HttpReply::error(error.status, "Image credit status is unavailable", None),
    }
    .with_header("Cache-Control", "no-store")
}

pub(crate) fn execute(
    disconnect: impl FnOnce() -> Option<crate::services::disconnect::DisconnectMonitor>,
    request: Request<'_>,
    body: Value,
    state: &ServerState,
    operation: &str,
) -> HttpReply {
    let mut incoming = request.incoming();
    if let Some(id) = request.observation_id {
        incoming.insert("X-EMP-Request-ID".into(), id.into());
    }
    let mut monitor = disconnect();
    crate::api::native_response::complete_response(images::execute(
        state,
        operation,
        &body,
        &incoming,
        request.hooks,
        monitor.as_mut(),
    ))
}
