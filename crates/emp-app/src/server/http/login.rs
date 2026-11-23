use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    app::ServerState,
    http::request::{Request, RequestMethod, read_json_body},
    server::{connection::ConnectionContext, run_blocking},
    services::native_login::{self, LoginOperation},
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    routing::post,
};
use serde_json::Value;
use std::{net::TcpStream, sync::Arc};

const NATIVE_PREFIX: &str = "/api/native/login/";
const ACCOUNTS_PREFIX: &str = "/api/accounts/login/";

pub(crate) fn matches(request: Request<'_>) -> bool {
    let path = request.raw_path();
    request.method == RequestMethod::Post
        && (path.starts_with(NATIVE_PREFIX) || path.starts_with(ACCOUNTS_PREFIX))
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    [NATIVE_PREFIX, ACCOUNTS_PREFIX]
        .into_iter()
        .fold(Router::new(), |router, prefix| {
            router
                .route(prefix, post(write))
                .route(&format!("{prefix}{{*path}}"), post(write))
        })
}

fn operation(path: &str, body: &Value) -> LoginOperation {
    let name = path.rsplit('/').next().unwrap_or_default();
    LoginOperation::parse(name, body, path.starts_with(ACCOUNTS_PREFIX))
}

pub(crate) fn legacy(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Result<JsonReply, EntryError> {
    ManagementSession::require(request, state, now)?;
    let body = read_json_body(stream, request, prefix, state)?;
    Ok(execute(operation(request.raw_path(), &body), state))
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

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(operation): JsonBody<LoginOperation>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || {
        let _connection = connection;
        execute(operation, &state)
    })
    .await
    .map_err(|_| EntryError::internal())
}

fn execute(operation: LoginOperation, state: &ServerState) -> JsonReply {
    match native_login::execute(state, operation) {
        Ok(value) => JsonReply::new(200, value.to_string().into_bytes())
            .with_header("Cache-Control", "no-store"),
        Err(code) => JsonReply::error(409, code, Some(code)).with_status_text("Conflict"),
    }
}
