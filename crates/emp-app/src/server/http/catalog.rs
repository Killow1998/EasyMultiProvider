use super::{
    body::JsonBody,
    error::{EntryError, JsonReply, no_response},
    management::{ManagementSession, write_route},
    proxy::{ModelCatalog, RequestAccess},
    raw_head,
};
use crate::{
    api::{
        catalog::{self, discovery_reply},
        decisions::{self, DecisionDiscovery},
        provider_quota, router_error_reply,
    },
    app::ServerState,
    http::request::percent_decode,
    server::run_blocking,
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
    routing::{MethodFilter, get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    use CatalogWriteRoute::*;

    let write = write_route(MethodFilter::POST, CatalogWrite::execute);
    Router::new()
        .route("/v1/models", get(models))
        .route("/v1/models/", get(models))
        .route("/v1/models/{*path}", get(models))
        .nest(
            "/api/providers",
            Router::new()
                .route("/balance", post(missing_balance))
                .route("/discover", write.clone().layer(Extension(Discover)))
                .route("/{id}/quota-history", get(provider_read))
                .fallback(
                    get(provider_read)
                        .merge(write.clone())
                        .fallback(super::unmatched),
                ),
        )
        .route(
            "/api/decisions/models",
            write.clone().layer(Extension(DecisionModels)),
        )
        .route(
            "/api/catalog/refresh",
            write.clone().layer(Extension(Refresh)),
        )
        .route(
            "/api/catalog/context-preference",
            write.clone().layer(Extension(ContextPreference)),
        )
        .route(
            "/api/models/metadata",
            write_route(MethodFilter::POST, |body, state, id| {
                catalog::metadata_request(id, state, &body)
            }),
        )
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
        let path = request
            .raw_path()
            .strip_prefix("/api/providers/")
            .ok_or(EntryError::NotFound)?;
        if let Some(id) = path.strip_suffix("/quota-history") {
            Ok(provider_quota::history(
                request,
                &state,
                id,
                crate::util::system_now(),
            ))
        } else if let Some(id) = path.strip_suffix("/models") {
            Ok(catalog::provider_models_request(Some(id), &state))
        } else if path == "models" {
            Ok(catalog::provider_models_request(None, &state))
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

#[derive(Clone)]
enum CatalogWriteRoute {
    Discover,
    DecisionModels,
    Refresh,
    ContextPreference,
    Quota(String),
    Balance(String),
}

impl CatalogWriteRoute {
    fn for_provider(path: &str) -> Option<Self> {
        let provider = path.strip_prefix("/api/providers/")?;
        if let Some(id) = provider.strip_suffix("/quota") {
            Some(Self::Quota(percent_decode(id, false)))
        } else if provider == "quota" {
            Some(Self::Quota(String::new()))
        } else {
            provider
                .strip_suffix("/balance")
                .map(|id| Self::Balance(percent_decode(id, false)))
        }
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
            Self::Balance(id) => CatalogWrite::Balance {
                id,
                force: force(body)?,
            },
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
            Self::Discover(operation) => discovery_reply(
                discovery::discover(request_id, state, operation),
                router_error_reply,
            ),
            Self::DecisionModels(operation) => discovery_reply(
                operation.and_then(|operation| decisions::discover(state, operation)),
                router_error_reply,
            ),
            Self::Refresh => match refresh_for_management(request_id, state) {
                Ok((path, count)) => JsonReply::ok_json(
                    &json!({"status":"ok","catalog_path":path,"model_count":count}),
                ),
                Err(()) => EntryError::internal().reply(),
            },
            Self::ContextPreference(show_context) => {
                match settings::context_preference(request_id, state, show_context) {
                    Ok(show_context) => {
                        JsonReply::ok_json(&json!({"catalog_show_context":show_context}))
                    }
                    Err(ChangeError::Invalid(message)) => JsonReply::error(400, &message, None),
                    Err(ChangeError::Unavailable) => EntryError::internal().reply(),
                }
            }
            Self::Quota { id, force } => provider_quota::refresh(state, &id, force),
            Self::Balance { id, force } => match provider_balance::refresh(state, &id, force) {
                Some(value) => JsonReply::ok_json(&value),
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
        let route = match request.extensions_mut().remove::<CatalogWriteRoute>() {
            Some(route) => route,
            None => CatalogWriteRoute::for_provider(
                raw_head(request.extensions())?.request().raw_path(),
            )
            .ok_or(EntryError::NotFound)?,
        };
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(route.with_body(&body)?))
    }
}
