//! Management endpoints for stable update discovery and installation.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use std::net::TcpStream;

pub(crate) fn read(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = serde_json::to_vec(&state.updates.snapshot()).unwrap_or_else(|_| b"{}".to_vec());
    response("HTTP/1.1 200 OK", "application/json", &body, &[])
}

pub(crate) fn start(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
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
