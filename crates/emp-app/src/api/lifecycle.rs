//! Authenticated shutdown, after restoring this process's owned integration.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use std::net::TcpStream;

pub(crate) fn quit_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> (Vec<u8>, bool) {
    if !same_origin(request, state.port) {
        return (
            cross_origin_response("management session is required"),
            false,
        );
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
    {
        return (unauthorized_response(), false);
    }
    if let Err(error) = read_json_body(stream, request, body_prefix, state) {
        return (body_error_response(error), false);
    }
    if state.backend.integration.restore_owned().is_err() {
        return (
            json_error_response(
                409,
                status_text(409),
                "Native configuration could not be restored; EMP is still running",
                None,
                &[],
            ),
            false,
        );
    }
    (
        response(
            "HTTP/1.1 200 OK",
            "application/json",
            br#"{"status":"stopping"}"#,
            &[],
        ),
        true,
    )
}
