use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::{ManagementRead, ManagementSession, read_route},
    proxy::RequestAccess,
};
use crate::{
    api::team,
    app::ServerState,
    http::request::{Request, RequestMethod},
    server::{connection::ConnectionContext, run_blocking},
    services::team::Command,
};
use axum::{Extension, Router, extract::State, routing::post};
use std::sync::Arc;

const READS: [(&str, ManagementRead); 2] = [
    ("/api/team/status", team::read),
    ("/api/team/notices", team::notices),
];
const PUBLISH_SOURCE: &str = "/api/team/publish_source";

pub(crate) fn read_handler(path: &str) -> Option<ManagementRead> {
    READS
        .iter()
        .find_map(|&(route, read)| (route == path).then_some(read))
}

pub(in crate::server) fn matches(request: Request<'_>) -> bool {
    match request.method {
        RequestMethod::Get => read_handler(request.raw_path()).is_some(),
        RequestMethod::Post => request.raw_path() == PUBLISH_SOURCE,
        _ => false,
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    READS.into_iter().fold(
        Router::new().route(PUBLISH_SOURCE, post(publish_source)),
        |router, (path, read)| router.route(path, read_route(read)),
    )
}

async fn publish_source(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(body): JsonBody,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || team::command(&state, Some(Command::PublishSource), &body))
        .await
        .map_err(|_| EntryError::internal())
}
