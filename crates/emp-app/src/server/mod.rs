pub(crate) mod connection;
pub(crate) mod head;
pub(crate) mod http;

use crate::app::ServerState;
use crate::lifecycle::connections::{ACCEPT_RETRY, AcceptLog, request_rejected};
use crate::services::connection_admission::ConnectionAdmission;
use axum::Router;
use std::{io, net::SocketAddr, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::watch,
    task::{JoinError, JoinHandle, JoinSet},
    time::{Instant, sleep},
};

#[cfg(test)]
#[path = "../../tests/support/raw_tcp.rs"]
mod raw_tcp;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownReason {
    Requested,
    ResponseWritten,
    ResponseWriteFailed,
}

#[derive(Clone)]
pub(crate) struct ShutdownSignal(watch::Receiver<Option<ShutdownReason>>);

impl ShutdownSignal {
    pub(crate) async fn wait(mut self) -> ShutdownReason {
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

struct ServerControl {
    mode: ServerMode,
    admission: Arc<ConnectionAdmission>,
    shutdown: watch::Sender<Option<ShutdownReason>>,
    candidate: Option<Arc<ServerState>>,
}

impl ServerControl {
    fn new(admission: Arc<ConnectionAdmission>) -> Self {
        Self {
            admission,
            mode: ServerMode::Http,
            shutdown: watch::Sender::new(None),
            candidate: None,
        }
    }

    fn signal(&self) -> ShutdownSignal {
        ShutdownSignal(self.shutdown.subscribe())
    }

    fn stop(&self, reason: ShutdownReason) {
        self.shutdown.send_if_modified(|state| {
            if state.is_some() {
                return false;
            }
            self.admission.close();
            *state = Some(reason);
            true
        });
    }
}

pub(crate) struct ServerHandle {
    address: SocketAddr,
    control: Arc<ServerControl>,
    listener: JoinHandle<Result<(), JoinError>>,
}

impl ServerHandle {
    pub(crate) async fn bind(
        address: SocketAddr,
        router: Router,
        admission: Arc<ConnectionAdmission>,
    ) -> io::Result<Self> {
        Self::bind_with_mode(address, router, admission, ServerMode::Http).await
    }

    pub(crate) async fn bind_recovery(
        address: SocketAddr,
        router: Router,
        admission: Arc<ConnectionAdmission>,
    ) -> io::Result<Self> {
        Self::bind_with_mode(address, router, admission, ServerMode::Recovery).await
    }

    async fn bind_with_mode(
        address: SocketAddr,
        router: Router,
        admission: Arc<ConnectionAdmission>,
        mode: ServerMode,
    ) -> io::Result<Self> {
        let socket = TcpListener::bind(address).await?;
        let address = socket.local_addr()?;
        let control = Arc::new(ServerControl {
            mode,
            ..ServerControl::new(admission)
        });
        let listener = tokio::spawn(serve(socket, router, Arc::clone(&control)));
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

    pub(crate) async fn shutdown(self) -> Result<(), JoinError> {
        self.control.stop(ShutdownReason::Requested);
        self.join().await
    }

    pub(crate) async fn join(mut self) -> Result<(), JoinError> {
        (&mut self.listener).await?
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.control.stop(ShutdownReason::Requested);
    }
}

pub(crate) async fn serve_candidate(
    listener: TcpListener,
    state: Arc<ServerState>,
) -> Result<(), JoinError> {
    let router = http::with_entry_state(
        http::accounts::routes()
            .merge(http::usage::routes())
            .merge(http::settings::routes())
            .merge(http::catalog::routes())
            .merge(http::updates::routes())
            .merge(http::modules::routes())
            .merge(http::gateway::routes()),
        state.clone(),
    );
    let control = Arc::new(ServerControl {
        candidate: Some(state.clone()),
        ..ServerControl::new(state.connection_admission.clone())
    });
    serve(listener, router, control).await
}

async fn serve(
    listener: TcpListener,
    router: Router,
    control: Arc<ServerControl>,
) -> Result<(), JoinError> {
    let mut connections = JoinSet::new();
    let mut worker_failure = None;
    let shutdown = control.signal();
    let mut accept_log = AcceptLog::default();
    let app_shutdown = async {
        match &control.candidate {
            Some(state) => state.wait_for_shutdown().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(app_shutdown);
    'accepting: loop {
        let accept = accept_with_retry(
            || listener.accept(),
            |accepted| {
                if let Some(state) = &control.candidate {
                    accept_log.record(state, accepted);
                }
            },
        );
        tokio::pin!(accept);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.clone().wait() => break 'accepting,
                _ = &mut app_shutdown => {
                    control.stop(ShutdownReason::Requested);
                    break 'accepting;
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        worker_failure.get_or_insert(error);
                    }
                }
                ((stream, peer), accepted) = &mut accept => {
                    if let Some(permit) = control.admission.acquire_request() {
                        if control.candidate.is_some() {
                            connections.spawn(connection::drive_candidate(
                                stream, peer, accepted, router.clone(), Arc::clone(&control), permit,
                            ));
                        } else if stream.set_nodelay(true).is_ok() {
                            connections.spawn(connection::drive(
                                stream, peer, accepted, router.clone(), Arc::clone(&control), permit,
                            ));
                        }
                    } else if let Some(state) = &control.candidate {
                        request_rejected(state, "connection_capacity");
                    }
                    break;
                }
            }
        }
    }
    drop(listener);
    while let Some(completed) = connections.join_next().await {
        if let Err(error) = completed {
            worker_failure.get_or_insert(error);
        }
    }
    worker_failure.map_or(Ok(()), Err)
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

pub(crate) async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, JoinError> {
    tokio::task::spawn_blocking(work).await
}
