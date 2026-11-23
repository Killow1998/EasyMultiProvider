use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::{ModelCatalog, RequestAccess},
    raw_head,
};
use crate::{
    api::catalog,
    app::ServerState,
    http::request::{Request, RequestMethod},
    server::{connection::ConnectionContext, run_blocking},
};
use axum::{
    Extension, Router,
    extract::State,
    routing::{get, post},
};
use std::sync::Arc;

pub(in crate::server) fn matches(request: Request<'_>) -> bool {
    let path = request.raw_path();
    match request.method {
        RequestMethod::Get => catalog::models_path(path) || catalog::provider_models_path(path),
        RequestMethod::Post => path == "/api/models/metadata",
        _ => false,
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/models/", get(models))
        .route("/v1/models/{*path}", get(models))
        .nest("/api/providers", Router::new().fallback(provider_models))
        .route("/api/models/metadata", post(metadata))
}

async fn models(
    _access: RequestAccess,
    _identity: ModelCatalog,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || catalog::models_request(raw.request(), &state))
        .await
        .map_err(|_| EntryError::internal())?
}

async fn provider_models(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || catalog::provider_models_request(raw.request().raw_path(), &state))
        .await
        .map_err(|_| EntryError::internal())
}

async fn metadata(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(body): JsonBody,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || catalog::metadata_request(connection.request_id(), &state, &body))
        .await
        .map_err(|_| EntryError::internal())
}
