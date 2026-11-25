use super::{
    error::{EntryError, JsonReply},
    management::{ManagementSession, read_route},
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::{catalog, inspection, integration, runtime},
    app::ServerState,
};
use axum::{Router, extract::State, routing::get};
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/config", get(read_configuration))
        .route(
            "/api/integration",
            read_route(integration::read_integration_request),
        )
        .route(
            "/api/runtime/claude-cli",
            read_route(|_| runtime::claude_cli_availability_request()),
        )
        .route(
            "/api/request-limits",
            read_route(inspection::request_limits),
        )
}

async fn read_configuration(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    crate::server::run_blocking(move || catalog::read_configuration(&state, raw.request().hooks))
        .await
        .map_err(|_| EntryError::internal())
}
