use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management, raw_head,
};
use crate::{
    app::ServerState,
    services::native_login::{self, LoginOperation},
};
use axum::{
    Router,
    extract::FromRequest,
    routing::{MethodFilter, MethodRouter},
};
use serde_json::Value;
use std::sync::Arc;

const NATIVE_PREFIX: &str = "/api/native/login/";
pub(super) const ACCOUNTS_PREFIX: &str = "/api/accounts/login/";

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    let write = write_route();
    Router::new()
        .route(NATIVE_PREFIX, write.clone())
        .route("/api/native/login/{*path}", write)
}

pub(super) fn write_route() -> MethodRouter<Arc<ServerState>> {
    management::write_route(MethodFilter::POST, |operation, state, _| {
        execute(operation, state)
    })
}

fn operation(path: &str, body: &Value) -> LoginOperation {
    let name = path.rsplit('/').next().unwrap_or_default();
    LoginOperation::parse(name, body, path.starts_with(ACCOUNTS_PREFIX))
}

impl FromRequest<Arc<ServerState>> for JsonBody<LoginOperation> {
    type Rejection = EntryError;

    async fn from_request(
        request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(request.extensions())?.clone();
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(operation(raw.request().raw_path(), &body)))
    }
}

fn execute(operation: LoginOperation, state: &ServerState) -> JsonReply {
    match native_login::execute(state, operation) {
        Ok(value) => JsonReply::new(200, value.to_string().into_bytes())
            .with_header("Cache-Control", "no-store"),
        Err(code) => JsonReply::error(409, code, Some(code)).with_status_text("Conflict"),
    }
}
