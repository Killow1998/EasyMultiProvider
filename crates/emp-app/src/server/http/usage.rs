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
    server::{handoff_response, run_blocking, spawn_response_work},
    services::{disconnect::DisconnectMonitor, events::sse_frame},
    util::system_now,
};
use axum::{
    Extension, Router,
    body::Bytes,
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
        write_progress: impl Fn(i64, i64) -> io::Result<()> + Send + 'static,
    ) -> Result<Vec<u8>, serde_json::Error> {
        #[cfg(test)]
        let gate = state.backend.usage.report_gate.clone();
        let progress = {
            #[cfg(test)]
            let cancelled = cancelled.clone();
            move |completed, total| {
                write_progress(completed, total)?;
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
        match report {
            Ok(report) => sse_frame("result", &report),
            Err(_) => sse_frame("error", &json!({"error":{"message":message}})),
        }
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
    let stream = request.extensions().get::<Arc<StreamConnection>>();
    let delivery = stream.map(|stream| stream.delivery.clone());
    let (sender, response) = oneshot::channel();
    let hooks = state.lifecycle.test_hooks.clone();
    let worker = spawn_response_work({
        let hooks = hooks.clone();
        move |business| {
            let Some(delivery) = delivery else {
                let response = report
                    .read(
                        raw.request(),
                        &state,
                        socket.as_ref().map(|Extension(socket)| socket.as_ref()),
                    )
                    .into_response();
                handoff_response(Some(business), || {
                    let _ = sender.send(response);
                    hooks.output_queued();
                });
                return;
            };
            let socket = socket.expect("report stream socket").0;
            let mut output = BodyOutput::new(socket, delivery, sender, business, hooks);
            let query = match report.prepare_stream(raw.request(), &state, system_now()) {
                Ok(query) => query,
                Err(reply) => {
                    output.finish(|| reply.into_response());
                    return;
                }
            };
            let Ok(mut monitor) = DisconnectMonitor::start(output.socket()) else {
                output.finish(no_response);
                return;
            };
            if output
                .head(200, "text/event-stream", std::iter::empty())
                .is_ok()
            {
                let output = Arc::new(Mutex::new(output));
                let progress_output = output.clone();
                let terminal =
                    query.stream(&state, monitor.cancellation(), move |completed, total| {
                        progress_output
                            .lock()
                            .expect("report output")
                            .report_progress(completed, total)
                    });
                if let Ok(terminal) = terminal {
                    let _ = output
                        .lock()
                        .expect("report output")
                        .finish_frame(Bytes::from(terminal), || ());
                }
            }
        }
    });
    if let Some(stream) = stream {
        *stream.worker.lock().expect("stream worker") = Some(worker);
    }
    hooks.before_response_receive().await;
    response.await.map_err(|_| EntryError::internal())
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
