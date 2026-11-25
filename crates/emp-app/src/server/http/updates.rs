use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::{ManagementSession, read_route},
    proxy::RequestAccess,
};
use crate::{api::updates, app::ServerState, server::run_blocking};
use axum::{Extension, Router, extract::State, routing::post};
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/updates", read_route(updates::read))
        .route("/api/updates/check", post(start).layer(Extension("check")))
        .route(
            "/api/updates/install",
            post(start).layer(Extension("install")),
        )
}

async fn start(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(operation): Extension<&'static str>,
    JsonBody(_): JsonBody,
) -> Result<JsonReply, EntryError> {
    run_blocking(move || updates::start_operation(&state, operation))
        .await
        .map_err(|_| EntryError::internal())
}
