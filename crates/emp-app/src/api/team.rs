use crate::server::http::error::JsonReply;
use crate::server::http::management::ManagementSession;
use crate::{
    app::ServerState,
    http::{
        request::{Request, read_json_body},
        response::body_error_response,
    },
    services::team::{Command, Failure},
};
use serde_json::Value;
use std::net::TcpStream;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    render(state.backend.team.local_status())
}

pub(crate) fn notices(state: &ServerState) -> JsonReply {
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
    command(state, Command::parse(operation), &body).wire_response()
}

pub(crate) fn command(state: &ServerState, command: Option<Command>, body: &Value) -> JsonReply {
    render(state.backend.team.command(state, command, body))
}

fn render(result: Result<Value, Failure>) -> JsonReply {
    match result {
        Ok(payload) => JsonReply::new(
            200,
            serde_json::to_vec(&payload).expect("workspace payload"),
        ),
        Err(Failure(status, code)) => JsonReply::error(
            status,
            "Workspace request could not be completed",
            Some(&code),
        ),
    }
    .with_header("Cache-Control", "no-store")
}
