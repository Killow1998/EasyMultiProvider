use super::{
    BUSINESS_TOKEN, ServerControl, ServerMode, ShutdownReason, ShutdownSignal,
    disconnect::WebSocketDisconnect,
    handoff_response,
    head::GuardedHead,
    http::{
        error::{BufferedResponse, EntryWire},
        inference::{BufferedReceipt, InferenceConnection, InferenceRoute},
        proxy::{AdmittedCallback, ConnectionIntake, RequestAccess},
        stream::{Flushed, StreamConnection},
        usage, websocket,
    },
    run_blocking, take_business,
    test_hooks::WriteHook,
    transport::ConnectionIo,
};
use crate::{
    app::ServerState,
    http::request::{
        MAX_HEADER_BYTES, REQUEST_HEAD_DEADLINE, RequestMethod, RequestMethods,
        module_runtime_target,
    },
    services::{
        connection_admission::{BusinessToken, ConnectionSlot},
        modules::aero::CallLease,
        observation::http::HttpObservation,
    },
};
use axum::{Router, extract::ConnectInfo, http::Uri};
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
    cell::RefCell,
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
    observation: HttpObservation,
    write_hook: WriteHook,
    delegation: Option<AdmittedCallback>,
    report_socket: Option<Arc<TcpStream>>,
    websocket_probe: Option<WebSocketDisconnect>,
    inference: Option<Arc<InferenceConnection>>,
    stream: Option<Arc<StreamConnection>>,
}

type UpgradeSession = Pin<Box<dyn Future<Output = ()> + Send>>;
type RunUpgrade = Box<
    dyn FnOnce(Option<TokioIo<Upgraded>>, Option<Arc<ConnectionSlot>>) -> UpgradeSession + Send,
>;

struct PendingUpgrade {
    io: OnUpgrade,
    reserved: Option<BusinessToken>,
    request: Option<BusinessToken>,
    run: RunUpgrade,
}

#[derive(Clone)]
pub(crate) struct ConnectionContext(Arc<Connection>);

struct Connection {
    control: Arc<ServerControl>,
    upgrade: Mutex<Option<PendingUpgrade>>,
    request_slot: Mutex<Option<Arc<ConnectionSlot>>>,
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
                .request_slot
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
    pub(crate) fn reserve_websocket(&self) -> Option<BusinessToken> {
        self.0.control.admission.acquire_websocket()
    }

    pub(crate) fn on_upgrade<F: Future<Output = ()> + Send + 'static>(
        &self,
        upgrade: OnUpgrade,
        reserved: Option<BusinessToken>,
        session: impl FnOnce(
            Option<TokioIo<Upgraded>>,
            Option<Arc<ConnectionSlot>>,
            ShutdownSignal,
        ) -> F
        + Send
        + 'static,
    ) {
        let shutdown = self.shutdown_signal();
        *self.0.upgrade.lock().expect("upgrade registration") = Some(PendingUpgrade {
            io: upgrade,
            reserved,
            request: None,
            run: Box::new(move |io, permit| Box::pin(session(io, permit, shutdown))),
        });
    }
}

pub(super) async fn drive_http(
    mut io: ConnectionIo,
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
    if io.set_nodelay().is_err() {
        let _ = run_blocking(move || observation.failure("socket_setup_failed")).await;
        return;
    }
    let head = GuardedHead::read(
        &mut io,
        accepted + REQUEST_HEAD_DEADLINE,
        RequestMethods::ExtendedFor(|target| module_runtime_target(target).is_some()),
    )
    .await;
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
            let _slot = permit.slot().clone();
            let bytes = error.reply().encode();
            drop(permit);
            let result = io.write_all(&bytes).await;
            let _ = io.shutdown().await;
            let _ = run_blocking(move || observation.response_finished(&bytes, result)).await;
            return;
        }
    };
    let request = head.raw.request();
    let responses_upgrade = websocket::is_responses_upgrade(request);
    let inference = InferenceRoute::for_request(request).is_some();
    let report = (request.method == RequestMethod::Get)
        .then(|| usage::Report::for_path(request.raw_path()))
        .flatten();
    let streaming =
        inference || head.account_events || report.is_some_and(|report| report.is_stream(request));
    let watch_report = report.is_some();
    observation.received(request);
    state
        .lifecycle
        .test_hooks
        .request(request.raw_path(), &observation, &io);
    crate::services::runtime::codex_contacted(request, &state);
    let socket = if watch_report || inference || responses_upgrade {
        match io.probe() {
            Ok(socket) => Some(socket),
            Err(_) if streaming => {
                let _ = run_blocking(move || observation.failure("socket_setup_failed")).await;
                return;
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let stream = streaming.then(|| Arc::new(StreamConnection::new(io.write_hook.clone())));
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
        (None, None, socket.and_then(WebSocketDisconnect::new))
    } else {
        (None, socket.map(Arc::new), None)
    };
    let http_context = HttpRequestContext {
        observation,
        write_hook: io.write_hook.clone(),
        delegation,
        report_socket,
        websocket_probe,
        inference,
        stream,
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
    permit: BusinessToken,
) {
    let mode = recovery_driver_mode(control.mode);
    let head = match GuardedHead::read(
        &mut io,
        accepted + mode.head_timeout(),
        RequestMethods::Standard,
    )
    .await
    {
        Ok(head) => head,
        Err(error) => {
            let _slot = permit.slot().clone();
            drop(permit);
            if matches!(mode, ServerMode::Http) {
                let _ = io.write_all(&error.reply().encode()).await;
            }
            let _ = io.shutdown().await;
            return;
        }
    };
    drive_connection(io, peer, router, control, permit, head, None).await;
}

#[cfg(test)]
fn recovery_driver_mode(mode: ServerMode) -> ServerMode {
    mode
}

#[cfg(not(test))]
fn recovery_driver_mode(_: ServerMode) -> ServerMode {
    ServerMode::Recovery
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
    permit: BusinessToken,
    head: GuardedHead,
    http_context: Option<HttpRequestContext>,
) {
    let mode = control.mode;
    let half_close =
        matches!(mode, ServerMode::Recovery) || (http_context.is_some() && !head.account_events);
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
    let write_hook = http_context.as_ref().map(|request| &request.write_hook);
    let request_id = http_context
        .as_ref()
        .and_then(|request| request.observation.request_id())
        .map(str::to_owned);
    let context = ConnectionContext(Arc::new(Connection {
        control,
        upgrade: Mutex::default(),
        request_slot: Mutex::new(permit.as_ref().map(|permit| permit.slot().clone())),
        request_access: OnceLock::new(),
        request_id,
        shutdown_after_response: Mutex::default(),
    }));
    let business = Mutex::new(permit);
    let encoded_reply = Arc::new(OnceLock::new());
    let unbuffered_status = OnceLock::new();
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
        let unbuffered_status = &unbuffered_status;
        let business = &business;
        let inference = inference.as_ref();
        let routing_uri = routing_uri.as_ref();
        async move {
            request.extensions_mut().insert(ConnectInfo(peer));
            request.extensions_mut().insert(raw.clone());
            request.extensions_mut().insert(context.clone());
            if let Some(delegation) = delegation {
                request.extensions_mut().insert(delegation.clone());
            }
            if let Some(socket) = report_socket {
                request.extensions_mut().insert(Arc::clone(socket));
            }
            if let Some(probe) = websocket_probe {
                request.extensions_mut().insert(probe.clone());
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
            let token = business.lock().expect("request work").take();
            let (mut response, mut token) = BUSINESS_TOKEN
                .scope(RefCell::new(token), async {
                    let response = router.oneshot(request).await?;
                    Ok::<_, std::convert::Infallible>((response, take_business()))
                })
                .await?;
            let flushed = response.extensions_mut().remove::<Flushed>();
            let local_upgrade = {
                let mut upgrade = context.0.upgrade.lock().expect("upgrade registration");
                if let Some(upgrade) = upgrade.as_mut() {
                    upgrade.request = token.take();
                }
                upgrade.is_some()
            };
            if let Some(flushed) = flushed
                && let Some(stream) = stream
            {
                let _ = unbuffered_status.set(response.status().as_u16());
                stream.delivery.await_flush(flushed, None);
            }
            if local_upgrade {
                let _ = unbuffered_status.set(response.status().as_u16());
            }
            let _ = encoded_reply.set(
                response
                    .extensions()
                    .get::<EntryWire>()
                    .map(|wire| wire.0.clone()),
            );
            if response.status().is_success() || local_upgrade {
                raw.allow_body();
            }
            if response.extensions().get::<BufferedResponse>().is_some() {
                Ok::<_, std::convert::Infallible>(handoff_response(token, || {
                    context
                        .0
                        .control
                        .test_hooks
                        .publish_output(write_hook, || response)
                }))
            } else {
                *business.lock().expect("request work") = token;
                Ok(response)
            }
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
    let result = connection.await.map_err(std::io::Error::other);
    let written = result.is_ok();
    let upgrade = context
        .0
        .upgrade
        .lock()
        .expect("upgrade registration")
        .take();
    if let Some(PendingUpgrade {
        io,
        reserved,
        request,
        run,
    }) = upgrade
    {
        let io = io.await.ok().map(TokioIo::new);
        let release_http_permit = async {
            drop(
                context
                    .0
                    .request_slot
                    .lock()
                    .expect("request admission")
                    .take(),
            );
            let control = &context.0.control;
            control
                .test_hooks
                .after_upgrade_handoff(&control.admission, http_context.is_some())
                .await;
        };
        let websocket_permit = reserved.or_else(|| {
            io.as_ref()
                .and_then(|_| context.0.control.admission.acquire_websocket())
        });
        drop(request);
        release_http_permit.await;
        let slot = websocket_permit.as_ref().map(|token| token.slot().clone());
        BUSINESS_TOKEN
            .scope(RefCell::new(websocket_permit), run(io, slot))
            .await;
    }
    drop(business.into_inner().expect("request work"));
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
                    } else if let Some(status) = unbuffered_status.into_inner() {
                        let bytes = delivery.as_ref().map(|delivery| delivery.bytes());
                        observation.response_delivery(Some(status), bytes, result.is_ok());
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
