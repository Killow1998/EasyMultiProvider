use super::{
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
};
use crate::{api::gateway::GatewayRead, app::ServerState, server::run_blocking};
use axum::{Router, extract::State, routing::get};
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/gateway", get(read))
        .route("/api/gateway/clients", get(read))
        .route("/api/gateway/usage", get(read))
        .route("/api/decisions/connection", get(read))
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    let read = GatewayRead::for_path(raw.request().raw_path()).ok_or(EntryError::NotFound)?;
    run_blocking(move || read.read(raw.request(), &state))
        .await
        .map_err(|_| EntryError::internal())
}
