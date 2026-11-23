use super::{
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::{calls, provider_quota, usage},
    app::ServerState,
    http::request::{Request, RequestMethod, query_values},
    server::run_blocking,
    services::disconnect::DisconnectMonitor,
    util::system_now,
};
use axum::{Extension, Router, extract::State, routing::get};
use std::{net::TcpStream, sync::Arc};

#[derive(Clone, Copy)]
pub(crate) enum Report {
    Usage,
    Calls,
}

impl Report {
    pub(crate) fn for_path(path: &str) -> Option<Self> {
        match path {
            "/api/usage" => Some(Self::Usage),
            "/api/calls" => Some(Self::Calls),
            _ => None,
        }
    }

    pub(crate) fn is_stream(self, request: Request<'_>) -> bool {
        request.header("Accept") == Some("text/event-stream")
            && (matches!(self, Self::Calls)
                || query_values(request.target, "lifetime").as_slice() == ["true"])
    }

    pub(crate) fn serve_legacy(
        self,
        stream: &mut TcpStream,
        request: Request<'_>,
        state: &ServerState,
        now: f64,
    ) -> Option<Vec<u8>> {
        if self.is_stream(request) {
            let _ = match self {
                Self::Usage => usage::lifetime::stream(stream, request, state, now),
                Self::Calls => calls::stream(stream, request, state, now),
            };
            return None;
        }
        Some(match ManagementSession::require(request, state, now) {
            Ok(_) => self.read(request, state, Some(stream)).wire_response(),
            Err(error) => error.wire_response(),
        })
    }

    fn read(
        self,
        request: Request<'_>,
        state: &ServerState,
        socket: Option<&TcpStream>,
    ) -> JsonReply {
        let mut monitor = socket.and_then(|socket| DisconnectMonitor::start(socket).ok());
        let cancelled = monitor.as_mut().map(DisconnectMonitor::cancellation);
        match self {
            Self::Usage => usage::read(request, state, cancelled),
            Self::Calls => calls::read(request, state, cancelled),
        }
    }
}

pub(in crate::server) fn matches(request: Request<'_>) -> bool {
    request.method == RequestMethod::Get
        && match Report::for_path(request.raw_path()) {
            Some(report) => !report.is_stream(request),
            None => provider_quota::history_id(request.raw_path())
                .is_some_and(|id| !id.is_empty() && !id.contains('/')),
        }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/usage", get(read_report))
        .route("/api/calls", get(read_report))
        .route(
            "/api/providers/{id}/quota-history",
            get(read_provider_quota),
        )
}

async fn read_report(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    socket: Option<Extension<Arc<TcpStream>>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || {
        let request = raw.request();
        Ok(Report::for_path(request.raw_path())
            .ok_or(EntryError::NotFound)?
            .read(
                request,
                &state,
                socket.as_ref().map(|Extension(socket)| socket.as_ref()),
            ))
    })
    .await
    .map_err(|_| EntryError::internal())?
}

async fn read_provider_quota(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<JsonReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || {
        let request = raw.request();
        let id = provider_quota::history_id(request.raw_path()).ok_or(EntryError::NotFound)?;
        Ok(provider_quota::history(request, &state, id, system_now()))
    })
    .await
    .map_err(|_| EntryError::internal())?
}
