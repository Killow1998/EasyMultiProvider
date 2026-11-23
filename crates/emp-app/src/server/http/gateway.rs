use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::gateway::{GatewayRead, json_reply},
    app::ServerState,
    http::request::{Request, RequestMethod},
    server::{connection::ConnectionContext, run_blocking},
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    routing::get,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const CLIENTS_PATH: &str = "/api/gateway/clients";

pub(crate) fn matches_write(request: Request<'_>) -> bool {
    matches!(request.method, RequestMethod::Post | RequestMethod::Delete)
        && request.raw_path() == CLIENTS_PATH
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/gateway", get(read))
        .route(CLIENTS_PATH, get(read).post(write).delete(write))
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

pub(crate) enum GatewayClientWrite {
    Issue {
        name: String,
        replace: Option<String>,
    },
    Remove {
        id: String,
    },
}

impl GatewayClientWrite {
    pub(crate) fn parse(body: Value, remove: bool) -> Result<Self, EntryError> {
        let missing_id = EntryError::Json(400, "A device key ID is required", None);
        if remove {
            let id = body["id"].as_str().ok_or(missing_id)?;
            return Ok(Self::Remove { id: id.to_owned() });
        }
        let replace = match body.get("id") {
            None => None,
            Some(Value::String(id)) => Some(id.clone()),
            Some(_) => return Err(missing_id),
        };
        Ok(Self::Issue {
            name: body["name"].as_str().unwrap_or_default().to_owned(),
            replace,
        })
    }

    pub(crate) fn execute(self, state: &ServerState) -> JsonReply {
        let backend = &state.backend;
        let clients = &backend.gateway_clients;
        let vault = &backend.configuration.vault;
        let modules = &backend.modules;
        let result = match self {
            Self::Issue { name, replace } => clients
                .issue(&name, replace.as_deref(), vault, modules)
                .map(|client| json_reply(&client)),
            Self::Remove { id } => clients
                .remove(&id, vault, modules)
                .map(|()| json_reply(&json!({"ok":true}))),
        };
        match result {
            Ok(reply) => reply,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                JsonReply::error(400, &error.to_string(), None)
            }
            Err(_) => JsonReply::error(503, "Gateway clients could not be saved", None),
        }
    }
}

impl FromRequest<Arc<ServerState>> for JsonBody<GatewayClientWrite> {
    type Rejection = EntryError;

    async fn from_request(
        request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let remove = request.method() == axum::http::Method::DELETE;
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(GatewayClientWrite::parse(body, remove)?))
    }
}

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(operation): JsonBody<GatewayClientWrite>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || {
        let _connection = connection;
        operation.execute(&state)
    })
    .await
    .map_err(|_| EntryError::internal())
}
