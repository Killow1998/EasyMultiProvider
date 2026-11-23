use crate::app::ServerState;
use crate::server::http::error::JsonReply;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    JsonReply::new(
        200,
        serde_json::to_vec(&state.backend.diagnostics.snapshot()).expect("diagnostic snapshot"),
    )
}
