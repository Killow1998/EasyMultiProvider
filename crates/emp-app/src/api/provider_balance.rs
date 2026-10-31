use crate::{
    app::ServerState,
    http::{
        auth::same_origin,
        request::{Request, percent_decode, read_json_body},
        response::*,
    },
};
use std::net::TcpStream;

pub(crate) fn refresh(
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
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let force = match body.get("force") {
        None => false,
        Some(serde_json::Value::Bool(force)) => *force,
        _ => return json_error_response(400, status_text(400), "force must be boolean", None, &[]),
    };
    let id = percent_decode(
        request
            .raw_path()
            .strip_prefix("/api/providers/")
            .expect("provider route")
            .strip_suffix("/balance")
            .expect("balance route"),
        false,
    );
    match crate::services::provider_balance::refresh(state, &id, force) {
        Some(value) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&value).expect("balance JSON"),
            &[],
        ),
        None => not_found_response(),
    }
}
