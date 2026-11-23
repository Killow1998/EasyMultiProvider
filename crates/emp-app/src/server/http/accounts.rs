use super::{
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::accounts::AccountRead,
    app::ServerState,
    http::request::{Request, RequestMethod},
    server::run_blocking,
    util::system_now,
};
use axum::{Router, extract::State, routing::get};
use std::sync::Arc;

pub(in crate::server) fn matches(request: Request<'_>) -> bool {
    request.method == RequestMethod::Get && AccountRead::for_path(request.raw_path()).is_some()
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/accounts", get(read))
        .route("/api/accounts/{*path}", get(read))
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || {
        let request = raw.request();
        AccountRead::for_path(request.raw_path())
            .ok_or(EntryError::NotFound)?
            .read(request, &state, system_now())
    })
    .await
    .map_err(|_| EntryError::internal())?
}
