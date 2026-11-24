use super::{
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::{catalog, inspection, integration, runtime},
    app::ServerState,
    server::run_blocking,
};
use axum::{Router, extract::State, routing::get};
use std::sync::Arc;

pub(crate) enum SettingsRead {
    Configuration,
    Integration,
    ClaudeCli,
    RequestLimits,
}

impl SettingsRead {
    pub(crate) fn for_path(path: &str) -> Option<Self> {
        match path {
            "/api/config" => Some(Self::Configuration),
            "/api/integration" => Some(Self::Integration),
            "/api/runtime/claude-cli" => Some(Self::ClaudeCli),
            "/api/request-limits" => Some(Self::RequestLimits),
            _ => None,
        }
    }

    pub(crate) fn read(self, state: &ServerState) -> JsonReply {
        match self {
            Self::Configuration => catalog::read_configuration(state),
            Self::Integration => integration::read_integration_request(state),
            Self::ClaudeCli => runtime::claude_cli_availability_request(),
            Self::RequestLimits => inspection::request_limits(state),
        }
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/config", get(read))
        .route("/api/integration", get(read))
        .route("/api/runtime/claude-cli", get(read))
        .route("/api/request-limits", get(read))
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let read = SettingsRead::for_path(raw_head(request.extensions())?.request().raw_path())
        .ok_or(EntryError::NotFound)?;
    run_blocking(move || read.read(&state))
        .await
        .map_err(|_| EntryError::internal())
}
