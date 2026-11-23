use super::http::error::EntryWire;
use super::{
    ServerControl, ServerMode, ShutdownReason, ShutdownSignal,
    head::GuardedHead,
    http::{accounts, settings, usage},
    run_blocking,
};
use crate::{
    http::request::{MAX_HEADER_BYTES, REQUEST_HEAD_DEADLINE},
    services::{connection_admission::ConnectionPermit, observation::http::HttpObservation},
};
use axum::{Router, extract::ConnectInfo, http::StatusCode};
use hyper::{
    server::conn::http1,
    upgrade::{OnUpgrade, Upgraded},
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::{
    net::{SocketAddr, TcpStream},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::{Duration, Instant},
};
use tower::ServiceExt;

#[derive(Clone, Copy)]
pub(crate) struct ShutdownAfterResponse;

struct CandidateRequest {
    observation: HttpObservation,
    report_socket: Option<Arc<TcpStream>>,
    half_close: bool,
}

type UpgradeSession = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone)]
pub(crate) struct ConnectionContext {
    control: Arc<ServerControl>,
    upgrade: Arc<Mutex<Option<UpgradeSession>>>,
}

impl ConnectionContext {
    pub(crate) fn shutdown_signal(&self) -> ShutdownSignal {
        self.control.signal()
    }

    pub(crate) fn reserve_websocket(&self) -> Option<ConnectionPermit> {
        self.control.admission.acquire_websocket()
    }

    pub(crate) fn on_upgrade<F: Future<Output = ()> + Send + 'static>(
        &self,
        upgrade: OnUpgrade,
        reserved: Option<ConnectionPermit>,
        session: impl FnOnce(TokioIo<Upgraded>, bool, ShutdownSignal) -> F + Send + 'static,
    ) {
        let control = Arc::clone(&self.control);
        let shutdown = self.shutdown_signal();
        let session = async move {
            if let Ok(io) = upgrade.await {
                let permit = reserved.or_else(|| control.admission.acquire_websocket());
                session(TokioIo::new(io), permit.is_some(), shutdown).await;
            }
        };
        *self.upgrade.lock().expect("upgrade registration") = Some(Box::pin(session));
    }
}

pub(super) async fn drive_candidate(
    mut io: tokio::net::TcpStream,
    peer: SocketAddr,
    accepted: Instant,
    router: Router,
    control: Arc<ServerControl>,
    permit: ConnectionPermit,
) {
    let state = control
        .candidate
        .as_ref()
        .expect("candidate listener state");
    let observation_state = Arc::clone(state);
    let Ok(mut observation) = run_blocking(move || HttpObservation::new(&observation_state)).await
    else {
        return;
    };
    if io.set_nodelay(true).is_err() {
        let _ = run_blocking(move || observation.failure("socket_setup_failed")).await;
        return;
    }
    let head = match GuardedHead::read(&mut io, accepted + REQUEST_HEAD_DEADLINE).await {
        Ok(head) => head,
        Err(error) => {
            let bytes = error.wire_response();
            let result = io.write_all(&bytes).await;
            let _ = io.shutdown().await;
            let _ = run_blocking(move || observation.response_finished(&bytes, result)).await;
            return;
        }
    };
    let request = head.raw.request();
    let usage_read = usage::matches(request);
    let settings_read = settings::matches(request);
    if !usage_read && !settings_read && !accounts::matches(request) {
        match io.into_std() {
            Ok(stream) => {
                let raw = head.into_legacy();
                crate::lifecycle::connections::spawn_request(state, move |state| {
                    crate::http::connection::handle_connection(
                        stream,
                        state,
                        permit,
                        Some(raw),
                        observation,
                    );
                });
            }
            Err(_) => {
                let _ = run_blocking(move || observation.failure("socket_setup_failed")).await;
            }
        }
        return;
    }
    observation.received(request);
    let (io, report_socket) = if usage_read && usage::Report::for_path(request.raw_path()).is_some()
    {
        match io.into_std().and_then(|stream| {
            let report_socket = stream.try_clone().ok().map(Arc::new);
            Ok((tokio::net::TcpStream::from_std(stream)?, report_socket))
        }) {
            Ok(connection) => connection,
            Err(_) => {
                let _ = run_blocking(move || observation.failure("socket_setup_failed")).await;
                return;
            }
        }
    } else {
        (io, None)
    };
    let candidate = CandidateRequest {
        observation,
        report_socket,
        half_close: usage_read || settings_read,
    };
    drive_http(io, peer, router, control, permit, head, Some(candidate)).await;
}

pub(super) async fn drive(
    mut io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    peer: SocketAddr,
    accepted: Instant,
    router: Router,
    control: Arc<ServerControl>,
    permit: ConnectionPermit,
) {
    let mode = control.mode;
    let head = match GuardedHead::read(&mut io, accepted + mode.head_timeout()).await {
        Ok(head) => head,
        Err(response) => {
            if matches!(mode, ServerMode::Http) {
                let _ = io.write_all(&response.wire_response()).await;
            }
            let _ = io.shutdown().await;
            return;
        }
    };
    drive_http(io, peer, router, control, permit, head, None).await;
}

impl ServerMode {
    fn head_timeout(self) -> Duration {
        match self {
            Self::Http => REQUEST_HEAD_DEADLINE,
            Self::Recovery => Duration::from_secs(3),
        }
    }
}

async fn drive_http(
    io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    peer: SocketAddr,
    router: Router,
    control: Arc<ServerControl>,
    permit: ConnectionPermit,
    head: GuardedHead,
    candidate: Option<CandidateRequest>,
) {
    let mode = control.mode;
    let half_close = candidate.as_ref().is_some_and(|request| request.half_close);
    let report_socket = candidate
        .as_ref()
        .and_then(|request| request.report_socket.as_ref());
    let permit = if head.release_request_permit {
        drop(permit);
        None
    } else {
        Some(permit)
    };
    let raw = head.raw.clone();
    let context = ConnectionContext {
        control,
        upgrade: Arc::default(),
    };
    let stop_after_write = AtomicBool::new(false);
    let wire_response = Arc::new(OnceLock::new());
    let service = tower::service_fn(|mut request: axum::http::Request<hyper::body::Incoming>| {
        let router = router.clone();
        let raw = raw.clone();
        let context = context.clone();
        let stop_after_write = &stop_after_write;
        let wire_response = &wire_response;
        async move {
            request.extensions_mut().insert(ConnectInfo(peer));
            request.extensions_mut().insert(raw.clone());
            request.extensions_mut().insert(context.clone());
            if let Some(socket) = report_socket {
                request.extensions_mut().insert(Arc::clone(socket));
            }
            let response = router.oneshot(request).await?;
            let _ = wire_response.set(
                response
                    .extensions()
                    .get::<EntryWire>()
                    .map(|wire| wire.0.clone()),
            );
            stop_after_write.store(
                response
                    .extensions()
                    .get::<ShutdownAfterResponse>()
                    .is_some(),
                Ordering::Relaxed,
            );
            if response.status().is_success()
                || response.status() == StatusCode::SWITCHING_PROTOCOLS
            {
                raw.allow_body();
            }
            if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                context.upgrade.lock().expect("upgrade registration").take();
            }
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    let result = http1::Builder::new()
        .keep_alive(false)
        .half_close(half_close)
        .allow_multiple_spaces_in_request_line_delimiters(true)
        .max_buf_size(MAX_HEADER_BYTES + 4)
        .max_headers(MAX_HEADER_BYTES / 4)
        .timer(TokioTimer::new())
        .header_read_timeout(mode.head_timeout())
        .auto_date_header(false)
        .serve_connection(
            TokioIo::new(head.replay(io, Arc::clone(&wire_response), mode)),
            TowerToHyperService::new(service),
        )
        .with_upgrades()
        .await;
    let written = result.is_ok();
    if let Some(CandidateRequest {
        mut observation, ..
    }) = candidate
    {
        let _ = run_blocking(move || {
            if let Some(Some(wire)) = wire_response.get() {
                observation.response_finished(wire, result.map_err(std::io::Error::other));
            }
        })
        .await;
    }

    if stop_after_write.into_inner() {
        context.control.stop(if written {
            ShutdownReason::ResponseWritten
        } else {
            ShutdownReason::ResponseWriteFailed
        });
    }
    let upgrade = context.upgrade.lock().expect("upgrade registration").take();
    if written && let Some(session) = upgrade {
        let _session_request_permit = permit;
        session.await;
    }
}
