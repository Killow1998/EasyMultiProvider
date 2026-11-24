use super::{
    body::JsonBody,
    error::{EntryError, JsonReply, no_response},
    management::ManagementSession,
    proxy::{ModelCatalog, RequestAccess},
    raw_head,
};
use crate::{
    api::{
        catalog::{self, catalog_json, discovery_reply},
        decisions::{self, DecisionDiscovery},
        provider_quota, router_error_reply,
    },
    app::ServerState,
    http::request::{Request, RequestMethod, percent_decode},
    server::{connection::ConnectionContext, run_blocking},
    services::{
        catalog::{
            discovery::{self, DiscoveryError, ProviderDiscovery},
            refresh_for_management,
        },
        configuration::{ChangeError, settings},
        provider_balance,
    },
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    response::Response,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/models/", get(models))
        .route("/v1/models/{*path}", get(models))
        .nest(
            "/api/providers",
            Router::new()
                .route("/balance", post(missing_balance))
                .fallback(get(provider_read).post(write).fallback(super::unmatched)),
        )
        .route("/api/decisions/models", post(write))
        .route("/api/catalog/refresh", post(write))
        .route("/api/catalog/context-preference", post(write))
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

async fn provider_read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || {
        let request = raw.request();
        let path = request.raw_path();
        if let Some(id) = provider_quota::history_id(path) {
            Ok(provider_quota::history(
                request,
                &state,
                id,
                crate::util::system_now(),
            ))
        } else if catalog::provider_models_path(path) {
            Ok(catalog::provider_models_request(path, &state))
        } else {
            Err(EntryError::NotFound)
        }
    })
    .await
    .map_err(|_| EntryError::internal())?
}

async fn missing_balance(
    _access: RequestAccess,
    _session: ManagementSession,
    JsonBody(body): JsonBody,
) -> Result<Response, EntryError> {
    force(&body)?;
    Ok(no_response())
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

#[derive(Clone)]
pub(crate) enum CatalogWriteRoute {
    Discover,
    DecisionModels,
    Refresh,
    ContextPreference,
    Quota(String),
    Balance(Option<String>),
}

impl CatalogWriteRoute {
    pub(crate) fn for_request(request: Request<'_>) -> Option<Self> {
        if request.method != RequestMethod::Post {
            return None;
        }
        let path = request.raw_path();
        Some(match path {
            "/api/providers/discover" => Self::Discover,
            "/api/decisions/models" => Self::DecisionModels,
            "/api/catalog/refresh" => Self::Refresh,
            "/api/catalog/context-preference" => Self::ContextPreference,
            _ => {
                let provider = path.strip_prefix("/api/providers/")?;
                if path.ends_with("/quota") {
                    Self::Quota(percent_decode(
                        provider.strip_suffix("/quota").unwrap_or_default(),
                        false,
                    ))
                } else if path.ends_with("/balance") {
                    Self::Balance(
                        provider
                            .strip_suffix("/balance")
                            .map(|id| percent_decode(id, false)),
                    )
                } else {
                    return None;
                }
            }
        })
    }

    fn with_body(self, body: &Value) -> Result<CatalogWrite, EntryError> {
        Ok(match self {
            Self::Discover => CatalogWrite::Discover(ProviderDiscovery::parse(body)),
            Self::DecisionModels => CatalogWrite::DecisionModels(DecisionDiscovery::parse(body)),
            Self::Refresh => CatalogWrite::Refresh,
            Self::ContextPreference => CatalogWrite::ContextPreference(context_preference(body)),
            Self::Quota(id) => CatalogWrite::Quota {
                id,
                force: force(body)?,
            },
            Self::Balance(id) => {
                let force = force(body)?;
                CatalogWrite::Balance {
                    id: id.expect("balance route"),
                    force,
                }
            }
        })
    }
}

enum CatalogWrite {
    Discover(Result<ProviderDiscovery, DiscoveryError>),
    DecisionModels(Result<DecisionDiscovery, DiscoveryError>),
    Refresh,
    ContextPreference(Result<bool, ChangeError>),
    Quota { id: String, force: bool },
    Balance { id: String, force: bool },
}

impl CatalogWrite {
    fn execute(self, state: &ServerState, request_id: Option<&str>) -> JsonReply {
        match self {
            Self::Discover(operation) => {
                discovery_reply(discovery::discover(request_id, state, operation))
                    .unwrap_or_else(router_error_reply)
            }
            Self::DecisionModels(operation) => discovery_reply(
                operation.and_then(|operation| decisions::discover(state, operation)),
            )
            .unwrap_or_else(router_error_reply),
            Self::Refresh => match refresh_for_management(request_id, state) {
                Ok((path, count)) => {
                    catalog_json(&json!({"status":"ok","catalog_path":path,"model_count":count}))
                }
                Err(()) => EntryError::internal().reply(),
            },
            Self::ContextPreference(show_context) => {
                match settings::context_preference(request_id, state, show_context) {
                    Ok(show_context) => catalog_json(&json!({"catalog_show_context":show_context})),
                    Err(ChangeError::Invalid(message)) => JsonReply::error(400, &message, None),
                    Err(ChangeError::Unavailable) => EntryError::internal().reply(),
                }
            }
            Self::Quota { id, force } => provider_quota::refresh(state, &id, force),
            Self::Balance { id, force } => match provider_balance::refresh(state, &id, force) {
                Some(value) => catalog_json(&value),
                None => EntryError::NotFound.reply(),
            },
        }
    }
}

fn context_preference(body: &Value) -> Result<bool, ChangeError> {
    if body.as_object().expect("JSON body is an object").len() != 1 {
        return Err(ChangeError::Invalid(
            "catalog preference request must contain only catalog_show_context".into(),
        ));
    }
    body["catalog_show_context"]
        .as_bool()
        .ok_or_else(|| ChangeError::Invalid("catalog_show_context must be boolean".into()))
}

fn force(body: &Value) -> Result<bool, EntryError> {
    match body.get("force") {
        None => Ok(false),
        Some(Value::Bool(force)) => Ok(*force),
        _ => Err(EntryError::Json(400, "force must be boolean", None)),
    }
}

impl FromRequest<Arc<ServerState>> for JsonBody<CatalogWrite> {
    type Rejection = EntryError;

    async fn from_request(
        mut request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let route = request
            .extensions_mut()
            .remove::<CatalogWriteRoute>()
            .ok_or(EntryError::NotFound)?;
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(route.with_body(&body)?))
    }
}

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(operation): JsonBody<CatalogWrite>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || operation.execute(&state, connection.request_id()))
        .await
        .map_err(|_| EntryError::internal())
}
