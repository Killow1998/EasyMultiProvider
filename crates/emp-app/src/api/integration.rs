use crate::app::ServerState;
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::http::auth::same_origin;
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::http::request::{Request, read_json_body};
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::http::response::{body_error_response, cross_origin_response, unauthorized_response};
use crate::server::http::error::JsonReply;
use crate::services::integration::commands::{self, Body, Reply};
#[cfg(any(test, not(feature = "server-candidate")))]
use serde_json::Value;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::net::TcpStream;

pub(crate) fn read_integration_request(state: &ServerState) -> JsonReply {
    render(commands::read(state))
}

#[cfg(any(test, not(feature = "server-candidate")))]
pub(crate) fn management_integration_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
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
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let operation = request.raw_path().rsplit('/').next().unwrap_or_default();
    let confirmed = body.get("confirm_reload") == Some(&Value::Bool(true));
    let reply = commands::execute(request.observation_id, state, operation, confirmed, 1);
    render(reply).wire_response()
}

pub(crate) fn render(reply: Reply) -> JsonReply {
    match reply.body {
        Body::Summary(value) => JsonReply::new(
            reply.status,
            serde_json::to_vec(&value).expect("integration summary"),
        ),
        Body::Unavailable => JsonReply::error(
            reply.status,
            "integration state is unavailable",
            (reply.status == 503).then_some("integration_unavailable"),
        ),
        Body::NotFound => JsonReply::error(404, "not found", None),
    }
}
