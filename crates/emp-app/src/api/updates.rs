use crate::app::ServerState;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{body_error_response, json_error_response, response, status_text};
use crate::server::http::error::JsonReply;
use crate::server::http::management::ManagementSession;
use std::net::TcpStream;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    let body = serde_json::to_vec(&state.updates.snapshot()).unwrap_or_else(|_| b"{}".to_vec());
    JsonReply::new(200, body)
}

pub(crate) fn start(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if let Err(error) = ManagementSession::require(request, state, now) {
        return error.wire_response();
    }
    if let Err(error) = read_json_body(stream, request, prefix, state) {
        return body_error_response(error);
    }
    let operation = request.raw_path().rsplit('/').next().unwrap_or_default();
    match state.updates.start(operation) {
        Ok(snapshot) => {
            let body = serde_json::to_vec(&snapshot).unwrap_or_else(|_| b"{}".to_vec());
            response("HTTP/1.1 202 Accepted", "application/json", &body, &[])
        }
        Err(code) => json_error_response(409, status_text(409), code, Some(code), &[]),
    }
}
