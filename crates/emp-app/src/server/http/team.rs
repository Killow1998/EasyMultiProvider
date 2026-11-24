use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::{ManagementRead, ManagementSession, read_route},
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::team,
    app::ServerState,
    http::request::{Request, RequestMethod, read_json_body},
    server::{connection::ConnectionContext, run_blocking},
    services::team::Command,
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    routing::post,
};
use serde_json::Value;
use std::{net::TcpStream, sync::Arc};

const READS: [(&str, ManagementRead); 2] = [
    ("/api/team/status", team::read),
    ("/api/team/notices", team::notices),
];
const WRITE_PREFIX: &str = "/api/team/";
const PUBLISH_SOURCE: &str = "/api/team/publish_source";

pub(crate) fn read_handler(path: &str) -> Option<ManagementRead> {
    READS
        .iter()
        .find_map(|&(route, read)| (route == path).then_some(read))
}

pub(crate) fn matches_write(request: Request<'_>) -> bool {
    request.method == RequestMethod::Post && request.raw_path().starts_with(WRITE_PREFIX)
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    READS.into_iter().fold(
        Router::new()
            .route(PUBLISH_SOURCE, post(publish_source))
            .route(WRITE_PREFIX, post(write))
            .route("/api/team/{*operation}", post(write)),
        |router, (path, read)| router.route(path, read_route(read).post(write)),
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

pub(crate) fn legacy(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Result<JsonReply, EntryError> {
    ManagementSession::require(request, state, now)?;
    let body = read_json_body(stream, request, prefix, state)?;
    Ok(team::command(state, parse(request.raw_path(), body)))
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

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(command): JsonBody<Command>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || {
        let _connection = connection;
        team::command(&state, command)
    })
    .await
    .map_err(|_| EntryError::internal())
}
