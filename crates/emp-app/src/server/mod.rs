mod accept;
pub(crate) mod connection;
pub(crate) mod head;
pub(crate) mod http;

use crate::app::ServerState;
use crate::services::{connection_admission::ConnectionAdmission, updates::UpdateState};
use accept::{ACCEPT_RETRY, AcceptLog, request_rejected};
use axum::{Router, middleware::from_fn_with_state};
#[cfg(test)]
use std::net::SocketAddr;
use std::{
    io,
    net::Shutdown,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinError, JoinHandle, JoinSet},
    time::{Duration, Instant, sleep, sleep_until},
};

#[cfg(test)]
#[path = "../../tests/support/raw_tcp.rs"]
pub(crate) mod raw_tcp;
#[cfg(test)]
mod tests;

pub(crate) const REQUEST_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    Requested,
    ResponseWritten,
    ResponseWriteFailed,
    RecoveryReset,
}

#[derive(Clone)]
pub(crate) struct ShutdownSignal(watch::Receiver<Option<(ShutdownReason, Instant)>>);

impl ShutdownSignal {
    pub(crate) async fn wait(self) -> ShutdownReason {
        self.wait_for_stop().await.0
    }

    async fn wait_for_stop(mut self) -> (ShutdownReason, Instant) {
        *self
            .0
            .wait_for(Option::is_some)
            .await
            .expect("server still owns the shutdown signal")
            .as_ref()
            .expect("shutdown was signaled")
    }
}

#[derive(Clone, Copy)]
enum ServerMode {
    Http,
    Recovery,
}

pub(crate) struct ServerControl {
    mode: ServerMode,
    pub(crate) admission: Arc<ConnectionAdmission>,
    pub(crate) shutdown: Arc<AtomicBool>,
    stopped: watch::Sender<Option<(ShutdownReason, Instant)>>,
    updates: Option<UpdateState>,
}

impl ServerControl {
    pub(crate) fn new(admission: Arc<ConnectionAdmission>) -> Self {
        Self {
            admission,
            mode: ServerMode::Http,
            shutdown: Arc::default(),
            stopped: watch::Sender::new(None),
            updates: None,
        }
    }

    pub(crate) fn with_updates(self, updates: UpdateState) -> Self {
        Self {
            updates: Some(updates),
            ..self
        }
    }

    pub(crate) fn updates(&self) -> &UpdateState {
        self.updates.as_ref().expect("normal application lifecycle")
    }

    pub(crate) fn recovery(admission: Arc<ConnectionAdmission>) -> Self {
        Self {
            mode: ServerMode::Recovery,
            ..Self::new(admission)
        }
    }

    pub(crate) fn signal(&self) -> ShutdownSignal {
        ShutdownSignal(self.stopped.subscribe())
    }

    pub(crate) fn stop(&self, reason: ShutdownReason) {
        self.stopped.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            self.admission.close();
            if let Some(updates) = &self.updates {
                updates.close();
            }
            self.shutdown.store(true, Ordering::Release);
            *state = Some((reason, Instant::now() + REQUEST_DRAIN_TIMEOUT));
            true
        });
    }
}

#[cfg(test)]
pub(crate) struct ServerHandle {
    address: SocketAddr,
    control: Arc<ServerControl>,
    listener: JoinHandle<Result<(), ServeError>>,
}

#[derive(Debug)]
pub(crate) enum ServeError {
    Accept(io::Error),
    Worker(JoinError),
    AdmissionNotDrained,
}

#[cfg(test)]
impl ServerHandle {
    pub(crate) async fn bind(
        address: SocketAddr,
        router: Router,
        admission: Arc<ConnectionAdmission>,
    ) -> io::Result<Self> {
        let socket = TcpListener::bind(address).await?;
        let address = socket.local_addr()?;
        let control = Arc::new(ServerControl::new(admission));
        let listener = tokio::spawn(serve(socket, router, Arc::clone(&control), None));
        Ok(Self {
            address,
            control,
            listener,
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn shutdown_signal(&self) -> ShutdownSignal {
        self.control.signal()
    }

    pub(crate) async fn shutdown(self) -> Result<(), ServeError> {
        self.control.stop(ShutdownReason::Requested);
        self.join().await
    }

    pub(crate) async fn join(mut self) -> Result<(), ServeError> {
        (&mut self.listener).await.map_err(ServeError::Worker)?
    }
}

#[cfg(test)]
impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.control.stop(ShutdownReason::Requested);
    }
}

pub(crate) async fn serve_http(
    listener: TcpListener,
    state: Arc<ServerState>,
) -> Result<(), ServeError> {
    let router = http_router(state.clone());
    serve(listener, router, state.lifecycle.clone(), Some(state)).await
}

pub(crate) async fn serve_recovery(
    listener: TcpListener,
    router: Router,
    control: Arc<ServerControl>,
) -> Result<(), ServeError> {
    serve(listener, router, control, None).await
}

fn http_router(state: Arc<ServerState>) -> Router {
    http::with_entry_state(
        http::accounts::routes()
            .route(
                http::management::SESSION_PATH,
                http::management::session_route(),
            )
            .merge(http::login::routes())
            .merge(http::local_writes::routes())
            .merge(http::usage::routes())
            .merge(http::settings::routes())
            .merge(http::catalog::routes())
            .merge(http::updates::routes())
            .merge(http::lifecycle::routes())
            .merge(http::modules::routes())
            .merge(http::gateway::routes())
            .merge(http::inspection::routes())
            .merge(http::pages::routes())
            .merge(http::team::routes())
            .merge(http::migration::routes())
            .merge(http::websocket::routes())
            .merge(declared_module_routes(&[]))
            .fallback(http::unmatched),
        state.clone(),
    )
    .merge(http::inference::routes())
    .method_not_allowed_fallback(http::unmatched)
    .layer(from_fn_with_state(state.clone(), http::guard_route_method))
    .with_state(state)
}

enum ModuleRoute {}

/// TODO(A2-S11): Check route conflicts when enabling or switching module declarations.
fn declared_module_routes(_declarations: &[ModuleRoute]) -> Router<Arc<ServerState>> {
    Router::new()
}

#[cfg(test)]
thread_local! {
    pub(crate) static RECOVERY_ACCEPT_FAILURE: std::cell::Cell<Option<io::ErrorKind>> =
        const { std::cell::Cell::new(None) };
}

async fn serve(
    listener: TcpListener,
    router: Router,
    control: Arc<ServerControl>,
    http_state: Option<Arc<ServerState>>,
) -> Result<(), ServeError> {
    let mut connections = JoinSet::new();
    let (blocking_work, pending_work) = watch::channel(());
    let (cancel_transport, cancelled) = watch::channel(());
    let mut worker_failure = None;
    let shutdown = control.signal();
    let mut accept_log = AcceptLog::default();
    let accept = || async {
        #[cfg(test)]
        if matches!(control.mode, ServerMode::Recovery)
            && let Some(error) = RECOVERY_ACCEPT_FAILURE.get()
        {
            return Err(error.into());
        }
        listener.accept().await
    };
    let deadline = 'accepting: loop {
        let accept = async {
            if matches!(control.mode, ServerMode::Recovery) {
                return accept()
                    .await
                    .map(|connection| (connection, Instant::now()));
            }
            Ok(accept_with_retry(accept, |accepted| {
                if let Some(state) = &http_state {
                    accept_log.record(state, accepted);
                }
            })
            .await)
        };
        tokio::pin!(accept);
        loop {
            let recovery_busy =
                matches!(control.mode, ServerMode::Recovery) && !connections.is_empty();
            tokio::select! {
                biased;
                (_, deadline) = shutdown.clone().wait_for_stop() => break 'accepting deadline,
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        worker_failure.get_or_insert(error);
                    }
                }
                result = &mut accept, if !recovery_busy => {
                    let ((stream, peer), accepted) = result.map_err(ServeError::Accept)?;
                    if let Some(state) = &http_state {
                        if let Some(intake) = http::proxy::ConnectionIntake::accept(state, peer.ip()) {
                            let state = state.clone();
                            let router = router.clone();
                            connections.spawn(BLOCKING_WORK.scope(pending_work.clone(), {
                                let cancelled = cancelled.clone();
                                async move {
                                    let result = drain_connection(stream, cancelled, |stream| {
                                        connection::drive_http(stream, peer, accepted, router, state.clone(), intake)
                                    }).await;
                                    if result.is_err() {
                                        request_rejected(&state, "socket_setup_failed");
                                    }
                                }
                            }));
                        } else {
                            request_rejected(state, "connection_capacity");
                        }
                    } else if let Some(permit) = control.admission.acquire_request()
                        && stream.set_nodelay(true).is_ok()
                    {
                        let router = router.clone();
                        let control = control.clone();
                        let cancelled = cancelled.clone();
                        connections.spawn(BLOCKING_WORK.scope(pending_work.clone(), async move {
                            let _ = drain_connection(stream, cancelled, |stream| {
                                connection::drive(stream, peer, accepted, router, control, permit)
                            }).await;
                        }));
                    }
                    break;
                }
            }
        }
    };
    drop(listener);
    drop(pending_work);
    let cutoff = sleep_until(deadline);
    tokio::pin!(cutoff);
    let mut transport_cancelled = false;
    while !connections.is_empty() {
        tokio::select! {
            () = &mut cutoff, if !transport_cancelled => {
                cancel_transport.send_replace(());
                transport_cancelled = true;
            }
            completed = connections.join_next() => {
                if let Some(Err(error)) = completed {
                    worker_failure.get_or_insert(error);
                }
            }
        }
    }
    blocking_work.closed().await;
    if let Some(error) = worker_failure {
        return Err(ServeError::Worker(error));
    }
    if !tokio::task::spawn_blocking(move || control.admission.wait_until_idle(deadline.into_std()))
        .await
        .map_err(ServeError::Worker)?
    {
        return Err(ServeError::AdmissionNotDrained);
    }
    Ok(())
}

async fn drain_connection<F: Future<Output = ()>>(
    stream: TcpStream,
    mut cancelled: watch::Receiver<()>,
    drive: impl FnOnce(TcpStream) -> F,
) -> io::Result<()> {
    let stream = stream.into_std()?;
    let socket = stream.try_clone()?;
    let drive = drive(TcpStream::from_std(stream)?);
    tokio::pin!(drive);
    tokio::select! {
        biased;
        _ = cancelled.changed() => {
            let _ = socket.shutdown(Shutdown::Both);
            drive.await;
        }
        () = &mut drive => {}
    }
    Ok(())
}

async fn accept_with_retry<T, F: Future<Output = io::Result<T>>>(
    mut accept: impl FnMut() -> F,
    mut record: impl FnMut(&io::Result<T>),
) -> (T, Instant) {
    loop {
        let result = accept().await;
        record(&result);
        match result {
            Ok(connection) => return (connection, Instant::now()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                tokio::task::consume_budget().await;
            }
            Err(_) => sleep(ACCEPT_RETRY).await,
        }
    }
}

tokio::task_local! {
    static BLOCKING_WORK: watch::Receiver<()>;
}

pub(crate) fn spawn_connection_work<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    let pending = BLOCKING_WORK.try_with(Clone::clone).ok();
    tokio::task::spawn_blocking(move || {
        let _pending = pending;
        work()
    })
}

pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, JoinError> {
    spawn_connection_work(work).await
}
