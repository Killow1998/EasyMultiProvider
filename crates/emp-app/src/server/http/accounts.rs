use super::{
    body::JsonBody,
    error::{EntryError, JsonReply, response_head},
    login,
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
    stream::{StreamConnection, StreamFrames},
};
use crate::{
    api::{
        accounts::{AccountRead, AccountWrite},
        quota,
    },
    app::ServerState,
    http::request::{Request, RequestMethod, percent_decode},
    server::{connection::ConnectionContext, run_blocking},
    services::{accounts::AccountImport, quota::commands::QuotaCommand},
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde_json::Value;
use std::sync::Arc;
use tokio::time::timeout;

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/accounts", get(read))
        .route("/api/accounts/events", get(events).delete(write))
        .route("/api/accounts/{*path}", get(read).post(write).delete(write))
        .route("/api/accounts/", delete(write))
        .route(login::ACCOUNTS_PREFIX, post(login::write).delete(write))
        .route(
            "/api/accounts/login/{*path}",
            get(read).post(login::write).delete(write),
        )
        .route("/api/config", post(write))
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
    let worker = tokio::spawn({
        let stream = stream.clone();
        async move {
            let mut subscription = subscription;
            let result = timeout(quota::EVENT_WRITE_TIMEOUT, output.flushed()).await;
            if result.is_err() {
                stream.write_timed_out.notify_one();
            }
            if !matches!(result, Ok(Ok(()))) {
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
                let result = timeout(quota::EVENT_WRITE_TIMEOUT, output.frame(frame)).await;
                if result.is_err() {
                    stream.write_timed_out.notify_one();
                }
                if !matches!(result, Ok(Ok(()))) || quit {
                    break;
                }
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
    run_blocking(move || {
        let request = raw.request();
        AccountRead::for_path(request.raw_path())
            .ok_or(EntryError::NotFound)?
            .read(request, &state, now)
    })
    .await
    .map_err(|_| EntryError::internal())?
}

#[derive(Clone)]
pub(crate) enum AccountWriteRoute {
    Import,
    Delete(String),
    EditConfiguration,
    RefreshModels(String),
    RefreshQuota(String),
    ResetQuota(String),
    MissingAccount,
}

impl AccountWriteRoute {
    pub(crate) fn for_request(request: Request<'_>) -> Option<Self> {
        let path = request.raw_path();
        if request.method == RequestMethod::Post && path == "/api/config" {
            return Some(Self::EditConfiguration);
        }
        let path = path.strip_prefix("/api/accounts/")?;
        match request.method {
            RequestMethod::Post if path.starts_with("login/") => None,
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

    fn without_body(&self) -> Result<Option<AccountWrite>, EntryError> {
        match self {
            Self::MissingAccount => Err(EntryError::NotFound),
            Self::Delete(account) => Ok(Some(AccountWrite::Delete(account.clone()))),
            _ => Ok(None),
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
        let route = request
            .extensions_mut()
            .remove::<AccountWriteRoute>()
            .ok_or(EntryError::NotFound)?;
        if let Some(command) = route.without_body()? {
            return Ok(Self(command));
        }
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(route.with_body(body)))
    }
}

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(command): JsonBody<AccountWrite>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || command.execute(&state, connection.request_id()))
        .await
        .map_err(|_| EntryError::internal())
}
