//! Integration HTTP adapter: authenticate, decode, execute, render.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::integration::commands::{self, Body, Reply};
use serde_json::Value;
use std::net::TcpStream;

pub(crate) fn read_integration_request(state: &ServerState) -> Vec<u8> {
    render(commands::read(state))
}

pub(crate) fn management_integration_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    stop_after_write: &mut bool,
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
    let reply = commands::execute(request.observation_id, state, operation, confirmed);
    *stop_after_write = reply.stop_after_write;
    render(reply)
}

fn render(reply: Reply) -> Vec<u8> {
    match reply.body {
        Body::Summary(value) => response(
            &format!("HTTP/1.1 {} {}", reply.status, status_text(reply.status)),
            "application/json",
            &serde_json::to_vec(&value).expect("integration summary"),
            &[],
        ),
        Body::Unavailable => json_error_response(
            reply.status,
            status_text(reply.status),
            "integration state is unavailable",
            (reply.status == 503).then_some("integration_unavailable"),
            &[],
        ),
        Body::NotFound => json_error_response(404, status_text(404), "not found", None, &[]),
    }
}
