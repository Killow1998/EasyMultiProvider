//! Standalone Codex image tools use their own JSON endpoints, outside Responses.
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::error::HttpReply;
use crate::services::native::images;
use serde_json::Value;
use std::net::TcpStream;

pub(crate) fn credit_status(state: &ServerState) -> HttpReply {
    match images::credit_status(state) {
        Ok(status) => HttpReply::new(200, status.to_string().into_bytes()),
        Err(error) => HttpReply::error(error.status, "Image credit status is unavailable", None),
    }
    .with_header("Cache-Control", "no-store")
}

pub(crate) fn execute(
    socket: &TcpStream,
    request: Request<'_>,
    body: Value,
    state: &ServerState,
    operation: &str,
) -> HttpReply {
    let mut incoming = request.incoming();
    if let Some(id) = request.observation_id {
        incoming.insert("X-EMP-Request-ID".into(), id.into());
    }
    let mut monitor = crate::services::disconnect::DisconnectMonitor::start(socket).ok();
    crate::api::native_response::complete_response(images::execute(
        state,
        operation,
        &body,
        &incoming,
        monitor.as_mut(),
    ))
}
