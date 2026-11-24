use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
};
use crate::{
    api::integration,
    app::ServerState,
    server::{ShutdownReason, connection::ConnectionContext, run_blocking},
    services::{integration::commands, shutdown::prepare_quit},
};
use axum::{Extension, Router, extract::State, routing::post};
use serde_json::Value;
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    ["enable", "restore", "reload", "verify"].into_iter().fold(
        Router::new().route("/api/quit", post(quit)),
        |router, operation| {
            router.route(
                &format!("/api/integration/{operation}"),
                post(integrate).layer(Extension(operation)),
            )
        },
    )
}

async fn integrate(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(context): Extension<ConnectionContext>,
    Extension(operation): Extension<&'static str>,
    JsonBody(body): JsonBody,
) -> Result<JsonReply, EntryError> {
    context.retain_request_access(access);
    let confirmed = body.get("confirm_reload") == Some(&Value::Bool(true));
    run_blocking(move || {
        integration::render(commands::execute(
            context.request_id(),
            &state,
            operation,
            confirmed,
            1,
        ))
    })
    .await
    .map_err(|_| EntryError::internal())
}

async fn quit(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(context): Extension<ConnectionContext>,
    JsonBody(_): JsonBody,
) -> Result<JsonReply, EntryError> {
    context.retain_request_access(access);
    let stop = context.shutdown_after_response();
    run_blocking(move || {
        prepare_quit(context.request_id(), &state, 1)
            .map_err(|error| EntryError::Json(409, error.message, error.code))?;
        let _ = stop.send(ShutdownReason::Requested);
        Ok(JsonReply::new(200, br#"{"status":"stopping"}"#.as_slice()))
    })
    .await
    .map_err(|_| EntryError::internal())?
}
