use super::{
    error::{EntryError, JsonReply, no_response},
    management::ManagementSession,
    proxy::RequestAccess,
    raw_head,
    stream::{BodyOutput, StreamConnection},
};
use crate::{
    api::{calls, provider_quota, usage},
    app::ServerState,
    http::request::{Request, query_values},
    server::run_blocking,
    services::{disconnect::DisconnectMonitor, events::sse_frame},
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::State,
    response::{IntoResponse, Response},
    routing::get,
};
use emp_state::usage::ledger::CallFilter;
use serde_json::json;
use std::{
    io,
    net::TcpStream,
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use tokio::sync::oneshot;

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

    fn prepare_stream(
        self,
        request: Request<'_>,
        state: &ServerState,
        now: f64,
    ) -> Result<ReportQuery, JsonReply> {
        Ok(match self {
            Self::Calls => ReportQuery::Calls(Box::new(calls::call_filter(request)?)),
            Self::Usage => {
                ReportQuery::Lifetime(usage::lifetime::stream_owner(request, state)?, now)
            }
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

#[cfg(test)]
pub(crate) struct ReportGate {
    pub(crate) first_frame: oneshot::Sender<Arc<AtomicBool>>,
    pub(crate) release: oneshot::Receiver<()>,
}

enum ReportQuery {
    Calls(Box<CallFilter>),
    Lifetime(String, f64),
}

impl ReportQuery {
    fn stream(
        self,
        state: &ServerState,
        cancelled: Arc<AtomicBool>,
        write_frame: impl Fn(&[u8]) -> io::Result<()> + Clone + Send + 'static,
    ) -> io::Result<()> {
        #[cfg(test)]
        let gate = state.backend.usage.report_gate.clone();
        let progress = {
            let write_frame = write_frame.clone();
            #[cfg(test)]
            let cancelled = cancelled.clone();
            move |completed, total| {
                write_frame(&sse_frame(
                    "progress",
                    &json!({"completed":completed,"total":total}),
                )?)?;
                #[cfg(test)]
                if completed == 0 {
                    let gate = gate.lock().unwrap().take();
                    if let Some(gate) = gate {
                        let _ = gate.first_frame.send(cancelled.clone());
                        let _ = gate.release.blocking_recv();
                    }
                }
                Ok(())
            }
        };
        let (report, message) = match self {
            Self::Calls(filter) => (
                calls::query(state, &filter, progress, Some(cancelled)),
                "Call history is unavailable",
            ),
            Self::Lifetime(owner, now) => (
                state.backend.usage.ledger.query_lifetime_with_progress(
                    &owner,
                    now,
                    progress,
                    Some(cancelled),
                ),
                "Usage history is unavailable",
            ),
        };
        let frame = match report {
            Ok(report) => sse_frame("result", &report)?,
            Err(_) => sse_frame("error", &json!({"error":{"message":message}}))?,
        };
        write_frame(&frame)
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
) -> Result<Response, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    let report = Report::for_path(raw.request().raw_path()).ok_or(EntryError::NotFound)?;
    if let Some(stream) = request.extensions().get::<Arc<StreamConnection>>() {
        let socket = socket.expect("report stream socket").0;
        let delivery = stream.delivery.clone();
        let (sender, response) = oneshot::channel();
        let worker = crate::server::spawn_connection_work(move || {
            let query = match report.prepare_stream(raw.request(), &state, system_now()) {
                Ok(query) => query,
                Err(reply) => {
                    let _ = sender.send(reply.into_response());
                    return;
                }
            };
            let Ok(mut monitor) = DisconnectMonitor::start(&socket) else {
                let _ = sender.send(no_response());
                return;
            };
            let mut output = BodyOutput::new(socket, delivery, sender);
            if output
                .head(200, "text/event-stream", std::iter::empty())
                .is_ok()
            {
                let output = Arc::new(Mutex::new(output));
                let _ = query.stream(&state, monitor.cancellation(), move |frame| {
                    output.lock().expect("report output").frame(frame)
                });
            }
        });
        *stream.worker.lock().expect("stream worker") = Some(worker);
        return response.await.map_err(|_| EntryError::internal());
    }
    run_blocking(move || {
        report
            .read(
                raw.request(),
                &state,
                socket.as_ref().map(|Extension(socket)| socket.as_ref()),
            )
            .into_response()
    })
    .await
    .map_err(|_| EntryError::internal())
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
