use super::management::read_route;
use crate::{
    api::{catalog, inspection, integration, runtime},
    app::ServerState,
};
use axum::Router;
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/config", read_route(catalog::read_configuration))
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
