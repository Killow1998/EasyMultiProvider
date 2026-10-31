use crate::{
    app::ServerState,
    http::{
        auth::same_origin,
        request::{Request, read_json_body},
        response::*,
    },
};
use std::net::TcpStream;

pub(crate) fn save(
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
    let selection = match serde_json::from_value(body) {
        Ok(selection) => selection,
        Err(_) => {
            return json_error_response(
                400,
                status_text(400),
                "app_icon_invalid",
                Some("app_icon_invalid"),
                &[],
            );
        }
    };
    match state.backend.app_icon.save(selection) {
        Ok(value) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&value).expect("icon JSON"),
            &[],
        ),
        Err(code) => json_error_response(
            if code == "app_icon_invalid" { 400 } else { 500 },
            status_text(if code == "app_icon_invalid" { 400 } else { 500 }),
            code,
            Some(code),
            &[],
        ),
    }
}
