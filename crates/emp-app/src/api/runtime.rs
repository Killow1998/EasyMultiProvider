//! Rescanning installed Codex runtimes, separate from shared runtime state.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::runtime::compatibility_snapshot;
use std::net::TcpStream;

pub(crate) fn management_request(
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
        .contains(request.session_token().as_deref(), now)
    {
        return unauthorized_response();
    }
    if let Err(error) = read_json_body(stream, request, prefix, state) {
        return body_error_response(error);
    }
    let value: Result<_, (u16, &str)> = Ok(compatibility_snapshot(state, true));
    match value {
        Ok(value) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&value).expect("runtime inventory JSON"),
            &[],
        ),
        Err((status, message)) => {
            json_error_response(status, status_text(status), message, None, &[])
        }
    }
}
