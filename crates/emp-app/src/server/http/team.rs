use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::{ManagementRead, ManagementSession, read_route, write_route},
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::team,
    app::ServerState,
    server::{connection::ConnectionContext, run_blocking},
    services::team::Command,
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    routing::{MethodFilter, post},
};
use serde_json::Value;
use std::sync::Arc;

const READS: [(&str, ManagementRead); 2] = [
    ("/api/team/status", team::read),
    ("/api/team/notices", team::notices),
];
const WRITE_PREFIX: &str = "/api/team/";
const PUBLISH_SOURCE: &str = "/api/team/publish_source";

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    let write = write_route(MethodFilter::POST, |command, state, _| {
        team::command(state, command)
    });
    READS.into_iter().fold(
        Router::new()
            .route(PUBLISH_SOURCE, post(publish_source))
            .route(WRITE_PREFIX, write.clone())
            .route("/api/team/{*operation}", write.clone()),
        |router, (path, read)| router.route(path, read_route(read).merge(write.clone())),
    )
}

async fn publish_source(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(command): JsonBody<Command>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || team::command(&state, command))
        .await
        .map_err(|_| EntryError::internal())
}

fn parse(path: &str, body: Value) -> Command {
    Command::parse(
        path.strip_prefix(WRITE_PREFIX).expect("team write path"),
        body,
    )
}

impl FromRequest<Arc<ServerState>> for JsonBody<Command> {
    type Rejection = EntryError;

    async fn from_request(
        request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(request.extensions())?.clone();
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(parse(raw.request().raw_path(), body)))
    }
}
