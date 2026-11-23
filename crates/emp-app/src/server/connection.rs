use super::{ServerControl, ShutdownReason, ShutdownSignal, head::GuardedHead};
use crate::{
    http::request::{MAX_HEADER_BYTES, REQUEST_HEAD_DEADLINE},
    services::connection_admission::ConnectionPermit,
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
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::Instant,
};
use tower::ServiceExt;

#[derive(Clone, Copy)]
pub(crate) struct ShutdownAfterResponse;

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

pub(super) async fn drive(
    mut io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
    peer: SocketAddr,
    accepted: Instant,
    router: Router,
    control: Arc<ServerControl>,
    permit: ConnectionPermit,
) {
    let head = match GuardedHead::read(&mut io, accepted).await {
        Ok(head) => head,
        Err(response) => {
            let _ = io.write_all(&response).await;
            let _ = io.shutdown().await;
            return;
        }
    };
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
    let service = tower::service_fn(|mut request: axum::http::Request<hyper::body::Incoming>| {
        let router = router.clone();
        let raw = raw.clone();
        let context = context.clone();
        let stop_after_write = &stop_after_write;
        async move {
            request.extensions_mut().insert(ConnectInfo(peer));
            request.extensions_mut().insert(raw);
            request.extensions_mut().insert(context.clone());
            let response = router.oneshot(request).await?;
            stop_after_write.store(
                response
                    .extensions()
                    .get::<ShutdownAfterResponse>()
                    .is_some(),
                Ordering::Relaxed,
            );
            if response.status() != StatusCode::SWITCHING_PROTOCOLS {
                context.upgrade.lock().expect("upgrade registration").take();
            }
            Ok::<_, std::convert::Infallible>(response)
        }
    });
    let written = http1::Builder::new()
        .keep_alive(false)
        .allow_multiple_spaces_in_request_line_delimiters(true)
        .max_buf_size(MAX_HEADER_BYTES + 4)
        .max_headers(MAX_HEADER_BYTES / 4)
        .timer(TokioTimer::new())
        .header_read_timeout(REQUEST_HEAD_DEADLINE)
        .auto_date_header(false)
        .serve_connection(
            TokioIo::new(head.replay(io)),
            TowerToHyperService::new(service),
        )
        .with_upgrades()
        .await
        .is_ok();

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
