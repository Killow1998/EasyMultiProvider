use crate::app::ServerState;
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::http::request::{Request, read_json_body};
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::http::response::body_error_response;
use crate::server::http::error::JsonReply;
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::server::http::management::ManagementSession;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::net::TcpStream;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    let body = serde_json::to_vec(&state.lifecycle.updates().snapshot())
        .unwrap_or_else(|_| b"{}".to_vec());
    JsonReply::new(200, body)
}

#[cfg(any(test, not(feature = "server-candidate")))]
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
    start_operation(state, operation).wire_response()
}

pub(crate) fn start_operation(state: &ServerState, operation: &str) -> JsonReply {
    match state.lifecycle.updates().start(operation) {
        Ok(snapshot) => {
            let body = serde_json::to_vec(&snapshot).unwrap_or_else(|_| b"{}".to_vec());
            JsonReply::new(202, body)
        }
        Err(code) => JsonReply::error(409, code, Some(code)),
    }
}
