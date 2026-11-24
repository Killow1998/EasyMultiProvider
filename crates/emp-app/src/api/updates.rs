use crate::app::ServerState;
use crate::server::http::error::JsonReply;
pub(crate) fn read(state: &ServerState) -> JsonReply {
    let body = serde_json::to_vec(&state.lifecycle.updates().snapshot())
        .unwrap_or_else(|_| b"{}".to_vec());
    JsonReply::new(200, body)
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
