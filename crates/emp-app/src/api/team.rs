use crate::{
    app::ServerState,
    server::http::error::JsonReply,
    services::team::{Command, Failure},
};
use serde_json::Value;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    render(state.backend.team.local_status())
}

pub(crate) fn notices(state: &ServerState) -> JsonReply {
    render(Ok(state.backend.team.notices()))
}

pub(crate) fn command(state: &ServerState, command: Command) -> JsonReply {
    render(state.backend.team.command(state, command))
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
