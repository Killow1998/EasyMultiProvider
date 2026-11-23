use crate::{app::ServerState, server::http::error::JsonReply};
use serde_json::json;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    JsonReply::new(
        200,
        serde_json::to_vec(&json!({
            "modules":state.backend.modules.status(),
            "migration_allowed":!state.backend.team.migration_restricted(),
        }))
        .expect("module status"),
    )
    .with_header("Cache-Control", "no-store")
}
