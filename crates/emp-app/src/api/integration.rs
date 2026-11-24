use crate::app::ServerState;
use crate::server::http::error::JsonReply;
use crate::services::integration::commands::{self, Body, Reply};
pub(crate) fn read_integration_request(state: &ServerState) -> JsonReply {
    render(commands::read(state))
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
