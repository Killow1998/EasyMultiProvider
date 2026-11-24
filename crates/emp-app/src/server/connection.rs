use super::{
    ServerControl, ServerMode, ShutdownReason, ShutdownSignal,
    head::GuardedHead,
    http::{
        accounts::AccountWriteRoute,
        catalog::CatalogWriteRoute,
        error::{BufferedResponse, EntryWire},
        inference::{BufferedReceipt, InferenceConnection, InferenceRoute},
        local_writes::LocalWriteRoute,
        proxy::{AdmittedCallback, ConnectionIntake, RequestAccess},
        stream::{Flushed, StreamConnection},
        usage, websocket,
    },
    run_blocking,
};
use crate::{
    app::ServerState,
    http::request::{MAX_HEADER_BYTES, REQUEST_HEAD_DEADLINE, RequestMethod},
    services::{
        connection_admission::ConnectionPermit, modules::aero::CallLease,
        observation::http::HttpObservation,
    },
};
use axum::{
    Router,
    extract::ConnectInfo,
    http::{StatusCode, Uri},
};
use hyper::{
    server::conn::http1,
    upgrade::{OnUpgrade, Upgraded},
};
use hyper_util::{
    rt::{TokioIo, TokioTimer},
    service::TowerToHyperService,
};
use std::{
    borrow::Cow,
    net::{SocketAddr, TcpStream},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::oneshot,
    time::{Duration, Instant},
};
use tower::ServiceExt;

struct HttpRequestContext {
    #[cfg(all(test, unix))]
    state: Arc<ServerState>,
    observation: HttpObservation,
    delegation: Option<AdmittedCallback>,
    report_socket: Option<Arc<TcpStream>>,
    websocket_probe: Option<Arc<tokio::net::TcpStream>>,
    inference: Option<Arc<InferenceConnection>>,
    stream: Option<Arc<StreamConnection>>,
    account_write: Option<AccountWriteRoute>,
    catalog_write: Option<CatalogWriteRoute>,
    local_write: Option<LocalWriteRoute>,
}

type UpgradeSession = Pin<Box<dyn Future<Output = ()> + Send>>;
type RunUpgrade = Box<
    dyn FnOnce(Option<TokioIo<Upgraded>>, Option<Arc<ConnectionPermit>>) -> UpgradeSession + Send,
>;

struct PendingUpgrade {
    io: OnUpgrade,
    reserved: Option<ConnectionPermit>,
    run: RunUpgrade,
}

#[derive(Clone)]
pub(crate) struct ConnectionContext(Arc<Connection>);

struct Connection {
    control: Arc<ServerControl>,
    upgrade: Mutex<Option<PendingUpgrade>>,
    request_permit: Mutex<Option<Arc<ConnectionPermit>>>,
    request_access: OnceLock<RequestAccess>,
    request_id: Option<String>,
    shutdown_after_response: Mutex<Option<oneshot::Receiver<ShutdownReason>>>,
}

impl ConnectionContext {
    pub(super) fn retain_request_access(&self, access: RequestAccess) {
        assert!(self.0.request_access.set(access).is_ok());
    }

    pub(super) fn aero_call_lease(&self) -> Option<CallLease> {
        Some(CallLease {
            connection: self
                .0
                .request_permit
                .lock()
                .expect("request admission")
                .as_ref()?
                .clone(),
            work: self.0.request_access.get()?.permit.as_ref()?.clone(),
        })
    }

    pub(super) fn request_id(&self) -> Option<&str> {
        self.0.request_id.as_deref()
    }

    pub(crate) fn shutdown_after_response(&self) -> oneshot::Sender<ShutdownReason> {
        let (sender, receiver) = oneshot::channel();
        *self
            .0
            .shutdown_after_response
            .lock()
            .expect("lifecycle response") = Some(receiver);
        sender
    }

    pub(crate) fn shutdown_signal(&self) -> ShutdownSignal {
        self.0.control.signal()
    }

    #[cfg(test)]
    pub(crate) fn reserve_websocket(&self) -> Option<ConnectionPermit> {
        self.0.control.admission.acquire_websocket()
    }

    pub(crate) fn on_upgrade<F: Future<Output = ()> + Send + 'static>(
        &self,
        upgrade: OnUpgrade,
        reserved: Option<ConnectionPermit>,
        session: impl FnOnce(
            Option<TokioIo<Upgraded>>,
            Option<Arc<ConnectionPermit>>,
            ShutdownSignal,
        ) -> F
        + Send
        + 'static,
    ) {
        let shutdown = self.shutdown_signal();
        *self.0.upgrade.lock().expect("upgrade registration") = Some(PendingUpgrade {
            io: upgrade,
            reserved,
            run: Box::new(move |io, permit| Box::pin(session(io, permit, shutdown))),
        });
    }
}

pub(super) async fn drive_http(
    mut io: tokio::net::TcpStream,
    peer: SocketAddr,
    accepted: Instant,
    router: Router,
    state: Arc<ServerState>,
    intake: ConnectionIntake,
) {
    let Ok(mut observation) = run_blocking({
        let state = state.clone();
        move || HttpObservation::new(&state)
    })
    .await
    else {
        return;
    };
    if io.set_nodelay(true).is_err() {
        let _ = run_blocking(move || observation.failure("socket_setup_failed")).await;
        return;
    }
    let head = GuardedHead::read(&mut io, accepted + REQUEST_HEAD_DEADLINE).await;
    let Some((permit, delegation)) = intake.admit(
        head.as_ref().ok().map(|head| head.raw.request()),
        &state,
        Some(peer.ip()),
    ) else {
        let _ = run_blocking(move || super::request_rejected(&state, "connection_capacity")).await;
        return;
    };
    let head = match head {
        Ok(head) => head,
        Err(error) => {
            let bytes = error.reply().encode();
            let result = io.write_all(&bytes).await;
            let _ = io.shutdown().await;
            let _ = run_blocking(move || observation.response_finished(&bytes, result)).await;
            return;
        }
    };
    let request = head.raw.request();
    let responses_upgrade = websocket::is_responses_upgrade(request);
    let account_write = AccountWriteRoute::for_request(request);
    let catalog_write = CatalogWriteRoute::for_request(request);
    let local_write = LocalWriteRoute::for_request(request);
    let inference = InferenceRoute::for_request(request).is_some();
    let report = (request.method == RequestMethod::Get)
        .then(|| usage::Report::for_path(request.raw_path()))
        .flatten();
    let streaming =
        inference || head.account_events || report.is_some_and(|report| report.is_stream(request));
    let watch_report = report.is_some();
    observation.received(request);
    crate::services::runtime::codex_contacted(request, &state);
    let (io, socket) = if watch_report || inference || responses_upgrade {
        match io.into_std().and_then(|stream| {
            let socket = stream.try_clone();
            let socket = if streaming {
                Some(socket?)
            } else {
                socket.ok()
            };
            Ok((tokio::net::TcpStream::from_std(stream)?, socket))
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
    let stream = streaming.then(|| Arc::new(StreamConnection::default()));
    let (inference, report_socket, websocket_probe) = if inference {
        (
            Some(Arc::new(InferenceConnection {
                socket: Arc::new(socket.expect("inference socket")),
                stream: stream.as_ref().expect("inference stream").clone(),
                model: Mutex::new(observation.model.take()),
                receipt: OnceLock::new(),
            })),
            None,
            None,
        )
    } else if responses_upgrade {
        let probe = socket
            .and_then(|socket| tokio::net::TcpStream::from_std(socket).ok())
            .map(Arc::new);
        (None, None, probe)
    } else {
        (None, socket.map(Arc::new), None)
    };
    let http_context = HttpRequestContext {
        #[cfg(all(test, unix))]
        state: state.clone(),
        observation,
        delegation,
        report_socket,
        websocket_probe,
        inference,
        stream,
        account_write,
        catalog_write,
        local_write,
    };
    drive_connection(
        io,
        peer,
        router,
        state.lifecycle.clone(),
        permit,
        head,
        Some(http_context),
    )
    .await;
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
                let _ = io.write_all(&response.reply().encode()).await;
            }
            let _ = io.shutdown().await;
            return;
        }
    };
    drive_connection(io, peer, router, control, Arc::new(permit), head, None).await;
}

impl ServerMode {
    fn head_timeout(self) -> Duration {
        match self {
            Self::Http => REQUEST_HEAD_DEADLINE,
            Self::Recovery => Duration::from_secs(3),
        }
    }
}

async fn drive_connection(
    io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    peer: SocketAddr,
    router: Router,
    control: Arc<ServerControl>,
    permit: Arc<ConnectionPermit>,
    head: GuardedHead,
    http_context: Option<HttpRequestContext>,
) {
    let mode = control.mode;
    let half_close =
        matches!(mode, ServerMode::Recovery) || (http_context.is_some() && !head.account_events);
    let account_write = http_context
        .as_ref()
        .and_then(|request| request.account_write.as_ref());
    let catalog_write = http_context
        .as_ref()
        .and_then(|request| request.catalog_write.as_ref());
    let local_write = http_context
        .as_ref()
        .and_then(|request| request.local_write);
    let report_socket = http_context
        .as_ref()
        .and_then(|request| request.report_socket.as_ref());
    let websocket_probe = http_context
        .as_ref()
        .and_then(|request| request.websocket_probe.as_ref());
    let inference = http_context
        .as_ref()
        .and_then(|request| request.inference.clone());
    let stream = http_context
        .as_ref()
        .and_then(|request| request.stream.as_ref());
    let delegation = http_context
        .as_ref()
        .and_then(|request| request.delegation.as_ref());
    let delivery = stream.map(|stream| stream.delivery.clone());
    let permit = if head.account_events {
        drop(permit);
        None
    } else {
        Some(permit)
    };
    let raw = head.raw.clone();
    let request_id = http_context
        .as_ref()
        .and_then(|request| request.observation.request_id())
        .map(str::to_owned);
    let context = ConnectionContext(Arc::new(Connection {
        control,
        upgrade: Mutex::default(),
        request_permit: Mutex::new(permit),
        request_access: OnceLock::new(),
        request_id,
        shutdown_after_response: Mutex::default(),
    }));
    let encoded_reply = Arc::new(OnceLock::new());
    let raw_request = raw.request();
    let (transport_target, routing_uri) = if http_context.is_some() {
        let uri = match super::http::routing_path(raw_request.target) {
            Cow::Borrowed(target) => Uri::try_from(target),
            Cow::Owned(target) => Uri::try_from(target),
        }
        .unwrap_or_else(|_| Uri::from_static("*"));
        ("/", Some(uri))
    } else {
        (raw_request.target, None)
    };
    let service = tower::service_fn(|mut request: axum::http::Request<hyper::body::Incoming>| {
        let router = router.clone();
        let raw = raw.clone();
        let context = context.clone();
        let encoded_reply = &encoded_reply;
        let inference = inference.as_ref();
        let routing_uri = routing_uri.as_ref();
        async move {
            request.extensions_mut().insert(ConnectInfo(peer));
            request.extensions_mut().insert(raw.clone());
            request.extensions_mut().insert(context.clone());
            if let Some(delegation) = delegation {
                request.extensions_mut().insert(delegation.clone());
            }
            if let Some(route) = account_write {
                request.extensions_mut().insert(route.clone());
            }
            if let Some(route) = catalog_write {
                request.extensions_mut().insert(route.clone());
            }
            if let Some(route) = local_write {
                request.extensions_mut().insert(route);
            }
            if let Some(socket) = report_socket {
                request.extensions_mut().insert(Arc::clone(socket));
            }
            if let Some(probe) = websocket_probe {
                request.extensions_mut().insert(Arc::clone(probe));
            }
            if let Some(inference) = inference {
                request.extensions_mut().insert(inference.clone());
            }
            if let Some(stream) = stream {
                request.extensions_mut().insert(stream.clone());
            }
            if let Some(uri) = routing_uri {
                *request.uri_mut() = uri.clone();
            }
            let mut response = router.oneshot(request).await?;
            if let Some(stream) = stream
                && let Some(flushed) = response.extensions_mut().remove::<Flushed>()
            {
                stream.delivery.await_flush(flushed);
            }
            if response.extensions().get::<BufferedResponse>().is_some()
                && delegation.is_none_or(|callback| callback.work.is_none())
                && let Some(permit) = context
                    .0
                    .request_permit
                    .lock()
                    .expect("request admission")
                    .as_ref()
            {
                permit.begin_buffered_delivery();
            }
            let _ = encoded_reply.set(
                response
                    .extensions()
                    .get::<EntryWire>()
                    .map(|wire| wire.0.clone()),
            );
            if response.status().is_success()
                || response.status() == StatusCode::SWITCHING_PROTOCOLS
            {
                raw.allow_body();
            }
            if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                context
                    .0
                    .upgrade
                    .lock()
                    .expect("upgrade registration")
                    .take();
            }
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    let websocket = websocket::is_upgrade_route(raw_request);
    let connection = http1::Builder::new()
        .keep_alive(websocket)
        .half_close(half_close)
        .title_case_headers(stream.is_some() || websocket)
        .max_buf_size(MAX_HEADER_BYTES + 4)
        .max_headers(MAX_HEADER_BYTES / 4)
        .timer(TokioTimer::new())
        .header_read_timeout(mode.head_timeout())
        .auto_date_header(false)
        .serve_connection(
            TokioIo::new(head.replay(
                io,
                transport_target,
                Arc::clone(&encoded_reply),
                mode,
                delivery.clone(),
            )),
            TowerToHyperService::new(service),
        )
        .with_upgrades();
    let result = tokio::select! {
        result = connection => result.map_err(std::io::Error::other),
        () = async {
            match stream {
                Some(stream) => stream.write_timed_out.notified().await,
                None => std::future::pending().await,
            }
        } => Err(std::io::ErrorKind::TimedOut.into()),
    };
    let written = result.is_ok();
    let upgrade = context
        .0
        .upgrade
        .lock()
        .expect("upgrade registration")
        .take();
    if let Some(PendingUpgrade { io, reserved, run }) = upgrade {
        let io = io.await.ok().map(TokioIo::new);
        let release_http_permit = async {
            drop(
                context
                    .0
                    .request_permit
                    .lock()
                    .expect("request admission")
                    .take(),
            );
            #[cfg(all(test, unix))]
            if let Some(request) = &http_context {
                let gate = request.state.test_upgrade_handoff.lock().unwrap().take();
                if let Some((reached, resume)) = gate {
                    let admission = &context.0.control.admission;
                    let _ =
                        reached.send((admission.active_requests(), admission.active_websockets()));
                    let _ = resume.await;
                }
            }
        };
        let websocket_permit = reserved
            .or_else(|| {
                io.as_ref()
                    .and_then(|_| context.0.control.admission.acquire_websocket())
            })
            .map(Arc::new);
        release_http_permit.await;
        run(io, websocket_permit.clone()).await;
        drop(websocket_permit);
    }
    if let Some(HttpRequestContext {
        mut observation,
        stream,
        ..
    }) = http_context
    {
        if let Some(stream) = stream {
            let worker = stream.worker.lock().expect("stream worker").take();
            if let Some(worker) = worker {
                let _ = worker.await;
            }
        }
        if let Some(inference) = &inference {
            observation.model = inference.model.lock().expect("model observation").take();
        }
        let _ = run_blocking(move || {
            match inference.as_ref().and_then(|request| request.receipt.get()) {
                Some(BufferedReceipt::Response { status, body }) => {
                    let result = if encoded_reply.get().is_none() {
                        Err(std::io::ErrorKind::BrokenPipe.into())
                    } else {
                        result
                    };
                    observation.buffered_finished(
                        Some(*status),
                        Some(body),
                        delivery.as_ref().expect("inference delivery").bytes(),
                        result,
                    );
                }
                Some(BufferedReceipt::Closed) => observation.response_finished(&[], Ok(())),
                None => {
                    if let Some(Some(wire)) = encoded_reply.get()
                        && (inference.is_none() || !wire.is_empty())
                    {
                        observation.response_finished(wire, result);
                    }
                }
            }
        })
        .await;
    }

    let deferred = context
        .0
        .shutdown_after_response
        .lock()
        .expect("lifecycle response")
        .take();
    if let Some(deferred) = deferred
        && let Ok(reason) = deferred.await
    {
        context.0.control.stop(match reason {
            ShutdownReason::Requested if written => ShutdownReason::ResponseWritten,
            ShutdownReason::Requested => ShutdownReason::ResponseWriteFailed,
            reason => reason,
        });
    }
}
