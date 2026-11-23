pub(crate) mod connection;
pub(crate) mod head;

use crate::lifecycle::connections::ACCEPT_RETRY;
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

struct ServerControl {
    admission: Arc<ConnectionAdmission>,
    shutdown: watch::Sender<Option<ShutdownReason>>,
}

impl ServerControl {
    fn new(admission: Arc<ConnectionAdmission>) -> Self {
        Self {
            admission,
            shutdown: watch::Sender::new(None),
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
        let socket = TcpListener::bind(address).await?;
        let address = socket.local_addr()?;
        let control = Arc::new(ServerControl::new(admission));
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

async fn serve(
    listener: TcpListener,
    router: Router,
    control: Arc<ServerControl>,
) -> Result<(), JoinError> {
    let mut connections = JoinSet::new();
    let mut worker_failure = None;
    let shutdown = control.signal();
    'accepting: loop {
        let accept = accept_with_retry(|| listener.accept());
        tokio::pin!(accept);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.clone().wait() => break 'accepting,
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        worker_failure.get_or_insert(error);
                    }
                }
                ((stream, peer), accepted) = &mut accept => {
                    if let Some(permit) = control.admission.acquire_request()
                        && stream.set_nodelay(true).is_ok()
                    {
                        connections.spawn(connection::drive(
                            stream, peer, accepted, router.clone(), Arc::clone(&control), permit,
                        ));
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
) -> (T, Instant) {
    loop {
        match accept().await {
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
