use super::{
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
};
use crate::{api::updates, app::ServerState, server::run_blocking};
use axum::{Router, extract::State, routing::get};
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new().route("/api/updates", get(read))
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
) -> Result<JsonReply, EntryError> {
    run_blocking(move || updates::read(&state))
        .await
        .map_err(|_| EntryError::internal())
}
