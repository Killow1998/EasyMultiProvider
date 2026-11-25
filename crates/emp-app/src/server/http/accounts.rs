use super::{
    body::JsonBody,
    error::{EntryError, JsonReply, response_head},
    login,
    management::{ManagementSession, write_route},
    proxy::RequestAccess,
    raw_head,
    stream::{StreamConnection, StreamFrames},
};
use crate::{
    api::{
        accounts::{self, AccountWrite},
        quota,
    },
    app::ServerState,
    http::request::{Request, RequestMethod, percent_decode},
    server::run_blocking,
    services::{accounts::AccountImport, quota::commands::QuotaCommand},
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    response::{IntoResponse, Response},
    routing::{MethodFilter, get},
};
use serde_json::Value;
use std::sync::Arc;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    let delete = write_route(MethodFilter::DELETE, AccountWrite::execute);
    let write = write_route(
        MethodFilter::POST.or(MethodFilter::DELETE),
        AccountWrite::execute,
    );
    let login = login::write_route();
    Router::new()
        .route("/api/accounts", get(read))
        .route("/api/accounts/events", get(events).merge(delete.clone()))
        .route("/api/accounts/{*path}", get(read).merge(write))
        .route("/api/accounts/", delete.clone())
        .route(login::ACCOUNTS_PREFIX, login.clone().merge(delete.clone()))
        .route(
            "/api/accounts/login/{*path}",
            get(read).merge(login).merge(delete),
        )
        .route(
            "/api/config",
            write_route(MethodFilter::POST, AccountWrite::execute)
                .layer(Extension(AccountWriteRoute::EditConfiguration)),
        )
}

async fn events(
    _access: RequestAccess,
    session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(stream): Extension<Arc<StreamConnection>>,
) -> Response {
    let Some(subscription) = state.backend.management_events.subscribe() else {
        return quota::subscriber_limit().into_response();
    };
    let (response, mut output) = StreamFrames::start(
        response_head(200, "text/event-stream")
            .header("cache-control", "no-store")
            .header("x-accel-buffering", "no"),
        stream.delivery.clone(),
    );
    let worker = tokio::spawn(async move {
        let mut subscription = subscription;
        if output.flushed().await.is_err() {
            return;
        }
        loop {
            let next = tokio::select! {
                biased;
                () = output.closed() => break,
                frame = quota::next_frame(&mut subscription, &session, &state) => frame,
            };
            let Some((frame, quit)) = next else {
                break;
            };
            if output.frame(frame).await.is_err() || quit {
                break;
            }
        }
    });
    *stream.worker.lock().expect("stream worker") = Some(worker);
    response
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let now = system_now();
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || accounts::read(raw.request(), &state, now))
        .await
        .map_err(|_| EntryError::internal())?
}

#[derive(Clone)]
enum AccountWriteRoute {
    Import,
    Delete(String),
    EditConfiguration,
    RefreshModels(String),
    RefreshQuota(String),
    ResetQuota(String),
    MissingAccount,
}

impl AccountWriteRoute {
    fn for_account(request: Request<'_>) -> Option<Self> {
        let path = request.raw_path().strip_prefix("/api/accounts/")?;
        match request.method {
            RequestMethod::Post if path == "import" || path == "models/refresh" => {
                Some(Self::Import)
            }
            RequestMethod::Post => {
                if let Some(account) = path.strip_suffix("/models/refresh") {
                    Some(Self::RefreshModels(percent_decode(account, false)))
                } else if let Some(account) = path.strip_suffix("/quota") {
                    Some(path_account(account).map_or(Self::MissingAccount, Self::RefreshQuota))
                } else if let Some(account) = path.strip_suffix("/quota-reset") {
                    Some(path_account(account).map_or(Self::MissingAccount, Self::ResetQuota))
                } else if matches!(path, "quota" | "quota-reset") {
                    Some(Self::MissingAccount)
                } else {
                    None
                }
            }
            RequestMethod::Delete => {
                Some(path_account(path).map_or(Self::MissingAccount, Self::Delete))
            }
            _ => None,
        }
    }

    fn with_body(self, body: Value) -> AccountWrite {
        match self {
            Self::Import => AccountWrite::Import(AccountImport::parse(&body)),
            Self::RefreshModels(account) => AccountWrite::RefreshModels(account),
            Self::RefreshQuota(account) => AccountWrite::Quota(account, QuotaCommand::Refresh),
            Self::ResetQuota(account) => AccountWrite::Quota(account, QuotaCommand::reset(&body)),
            Self::EditConfiguration => {
                let Value::Object(body) = body else {
                    unreachable!("JSON body reader requires an object")
                };
                AccountWrite::EditConfiguration(body)
            }
            Self::Delete(_) | Self::MissingAccount => unreachable!("route does not read a body"),
        }
    }
}

fn path_account(raw: &str) -> Option<String> {
    let raw = raw.trim_end_matches('/');
    (!raw.is_empty()).then(|| percent_decode(raw, false))
}

impl FromRequest<Arc<ServerState>> for JsonBody<AccountWrite> {
    type Rejection = EntryError;

    async fn from_request(
        mut request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let route = match request.extensions_mut().remove::<AccountWriteRoute>() {
            Some(route) => route,
            None => AccountWriteRoute::for_account(raw_head(request.extensions())?.request())
                .ok_or(EntryError::NotFound)?,
        };
        match route {
            AccountWriteRoute::MissingAccount => return Err(EntryError::NotFound),
            AccountWriteRoute::Delete(account) => return Ok(Self(AccountWrite::Delete(account))),
            _ => {}
        }
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(route.with_body(body)))
    }
}
