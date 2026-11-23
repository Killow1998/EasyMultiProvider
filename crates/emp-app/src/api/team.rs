use crate::server::http::management::ManagementSession;
use crate::{
    app::ServerState,
    http::{
        request::{Request, read_json_body},
        response::{body_error_response, json_error_response, response, status_text},
    },
    services::team::{Command, Failure},
};
use serde_json::Value;
use std::net::TcpStream;

pub(crate) fn read(state: &ServerState) -> Vec<u8> {
    render(state.backend.team.local_status())
}

pub(crate) fn notices(state: &ServerState) -> Vec<u8> {
    render(Ok(state.backend.team.notices()))
}

pub(crate) fn mutate(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if let Err(error) = ManagementSession::require(request, state, now) {
        return error.wire_response();
    }
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let operation = request
        .raw_path()
        .strip_prefix("/api/team/")
        .expect("team path");
    render(
        state
            .backend
            .team
            .command(state, Command::parse(operation), &body),
    )
}

fn render(result: Result<Value, Failure>) -> Vec<u8> {
    let headers = [("Cache-Control", "no-store")];
    match result {
        Ok(payload) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&payload).expect("workspace payload"),
            &headers,
        ),
        Err(Failure(status, code)) => json_error_response(
            status,
            status_text(status),
            "Workspace request could not be completed",
            Some(&code),
            &headers,
        ),
    }
}
