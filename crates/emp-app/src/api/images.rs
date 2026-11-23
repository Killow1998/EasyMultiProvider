//! Standalone Codex image tools use their own JSON endpoints, outside Responses.
use crate::app::ServerState;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{body_error_response, json_error_response, response, status_text};
use crate::server::http::proxy::require_proxy;
use crate::services::native::images;
use std::net::TcpStream;

pub(crate) fn credit_status(state: &ServerState) -> Vec<u8> {
    let headers = [("Cache-Control", "no-store")];
    match images::credit_status(state) {
        Ok(status) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            status.to_string().as_bytes(),
            &headers,
        ),
        Err(error) => json_error_response(
            error.status,
            status_text(error.status),
            "Image credit status is unavailable",
            None,
            &headers,
        ),
    }
}

pub(crate) fn image_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if let Err(error) = require_proxy(request, state, now) {
        return error.wire_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let mut incoming = request.incoming();
    if let Some(id) = request.observation_id {
        incoming.insert("X-EMP-Request-ID".into(), id.into());
    }
    let operation = if request.raw_path().ends_with("/edits") {
        "edits"
    } else {
        "generations"
    };
    let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
    crate::api::native_response::complete_response(images::execute(
        state,
        operation,
        &body,
        &incoming,
        monitor.as_mut(),
    ))
}
