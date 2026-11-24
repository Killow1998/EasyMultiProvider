use super::{
    connection::{ConnectionContext, ShutdownAfterResponse, drive},
    head::RawRequestHead,
    raw_tcp::{DEADLINE, RawClient},
    *,
};
use crate::services::connection_admission::{ConnectionAdmissionConfig, SlotLimits};
use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{ConnectInfo, Request},
    http::{Response, StatusCode},
    routing::get,
};
use hyper::body::{Body as HttpBody, Frame};
use std::{
    convert::Infallible,
    future::poll_fn,
    net::{Ipv4Addr, SocketAddr},
    pin::{Pin, pin},
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt, duplex},
    net::TcpStream,
    sync::oneshot,
    time::{Duration, advance, timeout},
};

pub(super) const LOOPBACK: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";

pub(super) fn admission() -> Arc<ConnectionAdmission> {
    let single = SlotLimits {
        initial: 1,
        maximum: 1,
        growth: 1,
    };
    Arc::new(ConnectionAdmission::new(ConnectionAdmissionConfig {
        requests: single,
        websockets: single,
    }))
}

pub(super) async fn poll_once<T>(mut future: Pin<&mut impl Future<Output = T>>) -> Poll<T> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

#[tokio::test(start_paused = true)]
async fn accept_retries_interrupted_immediately_and_other_errors_after_backoff() {
    let start = Instant::now();
    let mut attempts = Vec::new();
    let mut failures = [
        io::ErrorKind::Interrupted,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::PermissionDenied,
    ]
    .into_iter();
    let (accepted, accepted_at) = accept_with_retry(
        || {
            attempts.push(Instant::now() - start);
            std::future::ready(
                failures
                    .next()
                    .map_or(Ok("socket"), |kind| Err(kind.into())),
            )
        },
        |_| {},
    )
    .await;
    assert_eq!(accepted, "socket");
    let retry = Duration::from_millis(100);
    assert_eq!(
        attempts,
        [Duration::ZERO, Duration::ZERO, retry, retry * 2, retry * 3]
    );
    assert_eq!(accepted_at - start, retry * 3);
}

#[tokio::test]
async fn listener_bind_failure_returns_immediately() {
    let listening = TcpListener::bind(LOOPBACK).await.unwrap();
    let address = listening.local_addr().unwrap();
    let result = timeout(
        DEADLINE,
        ServerHandle::bind(address, Router::new(), admission()),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::AddrInUse));
}

#[tokio::test(start_paused = true)]
async fn idle_and_trickling_heads_hold_permits_until_their_deadlines() {
    for (initial, trickle) in [
        (b"".as_slice(), false),
        (b"GET / HTTP/1.1\r\nX: ".as_slice(), false),
        (b"GET / HTTP/1.1\r\nX: ".as_slice(), true),
    ] {
        let admission = admission();
        let control = Arc::new(ServerControl::new(Arc::clone(&admission)));
        let permit = admission.acquire_request().unwrap();
        let start = Instant::now();
        let (mut client, io) = duplex(1024);
        client.write_all(initial).await.unwrap();
        if trickle {
            advance(Duration::from_secs(1)).await;
        }
        let mut driver = pin!(drive(io, LOOPBACK, start, Router::new(), control, permit));
        assert!(poll_once(driver.as_mut()).await.is_pending());
        assert!(admission.acquire_request().is_none());

        let expires = Duration::from_secs(if trickle { 30 } else { 5 });
        if trickle {
            for _ in 0..7 {
                advance(Duration::from_secs(4)).await;
                client.write_all(b"x").await.unwrap();
                assert!(poll_once(driver.as_mut()).await.is_pending());
                assert!(admission.acquire_request().is_none());
            }
        }
        let tick = Duration::from_millis(1);
        advance(expires - (Instant::now() - start) - tick).await;
        assert!(poll_once(driver.as_mut()).await.is_pending());
        assert_eq!(admission.active_requests(), 1);
        advance(tick).await;
        driver.await;
        assert_eq!(Instant::now() - start, expires);
        assert_eq!(admission.active_requests(), 0);
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, crate::http::response::bad_request_response());
    }
}

#[tokio::test]
async fn idle_tcp_connections_already_count_against_admission() {
    let admission = admission();
    let server = ServerHandle::bind(
        LOOPBACK,
        Router::new().route("/", get(|| async { "ready" })),
        Arc::clone(&admission),
    )
    .await
    .unwrap();
    let mut first = TcpStream::connect(server.local_addr()).await.unwrap();
    let mut second = TcpStream::connect(server.local_addr()).await.unwrap();
    let (mut a, mut b) = ([0], [0]);
    let survivor = timeout(DEADLINE, async {
        tokio::select! {
            closed = first.read(&mut a) => { assert_eq!(closed.unwrap(), 0); second }
            closed = second.read(&mut b) => { assert_eq!(closed.unwrap(), 0); first }
        }
    })
    .await
    .expect("a connection is refused before either sends a request");
    assert_eq!(admission.active_requests(), 1);
    drop(survivor);
    server.shutdown().await.unwrap();
    assert_eq!(admission.active_requests(), 0);
}

struct GatedBody {
    first: bool,
    tail: oneshot::Receiver<()>,
}

impl HttpBody for GatedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if std::mem::take(&mut self.first) {
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"first")))));
        }
        let _ = ready!(Pin::new(&mut self.tail).poll(cx));
        Poll::Ready(None)
    }
}

#[tokio::test]
async fn response_body_holds_admission_until_completion_or_client_disconnect() {
    for (target, exempt, disconnect) in [
        ("/api/proxy/events", false, false),
        ("/api/proxy/events", false, true),
        ("/api/accounts/events?watch=1", true, false),
    ] {
        let admission = admission();
        let (mut release, tail) = oneshot::channel();
        let tail = Arc::new(std::sync::Mutex::new(Some(tail)));
        let router = Router::new().route("/", get(|| async { "ready" })).route(
            target.split('?').next().unwrap(),
            get(move || async move {
                let tail = tail.lock().unwrap().take().unwrap();
                Response::builder()
                    .header("Content-Type", "text/event-stream")
                    .body(Body::new(GatedBody { first: true, tail }))
                    .unwrap()
            }),
        );
        let server = ServerHandle::bind(LOOPBACK, router, Arc::clone(&admission))
            .await
            .unwrap();
        let mut client = RawClient::request(
            server.local_addr(),
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
        )
        .await;
        assert!(client.head.starts_with("HTTP/1.1 200"));
        let mut first = [0; 10];
        timeout(DEADLINE, client.io.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&first, b"5\r\nfirst\r\n");
        assert_eq!(admission.active_requests(), usize::from(!exempt));
        if exempt {
            let mut next = RawClient::request(server.local_addr(), GET).await;
            assert_eq!(next.rest().await, b"ready");
            assert!(!release.is_closed());
        } else {
            let mut refused = TcpStream::connect(server.local_addr()).await.unwrap();
            assert_eq!(
                timeout(DEADLINE, refused.read_u8())
                    .await
                    .unwrap()
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }

        if disconnect {
            drop(client);
            timeout(DEADLINE, release.closed()).await.unwrap();
        } else {
            release.send(()).unwrap();
            assert_eq!(client.rest().await, b"0\r\n\r\n");
        }
        assert_eq!(admission.active_requests(), 0);
        let mut next = RawClient::request(server.local_addr(), GET).await;
        assert_eq!(next.rest().await, b"ready");
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn upgrade_releases_request_permit_and_graceful_shutdown_waits_for_session() {
    for reserve_before_handshake in [false, true] {
        let admission = admission();
        let router =
            Router::new().route(
                "/",
                get(
                    move |Extension(context): Extension<ConnectionContext>,
                          mut request: Request| async move {
                        let reserved = if reserve_before_handshake {
                            Some(context.reserve_websocket().unwrap())
                        } else {
                            None
                        };
                        context.on_upgrade(
                            hyper::upgrade::on(&mut request),
                            reserved,
                            |socket, admitted, shutdown| async move {
                                let mut socket = socket.unwrap();
                                assert!(admitted);
                                assert_eq!(socket.read_u8().await.unwrap(), b'x');
                                socket.write_all(b"ready").await.unwrap();
                                assert_eq!(shutdown.wait().await, ShutdownReason::Requested);
                                socket.write_all(b"stopping").await.unwrap();
                                assert_eq!(socket.read_u8().await.unwrap(), b'y');
                            },
                        );
                        Response::builder()
                            .status(StatusCode::SWITCHING_PROTOCOLS)
                            .header("Connection", "Upgrade")
                            .header("Upgrade", "websocket")
                            .header("Sec-WebSocket-Accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
                            .body(Body::empty())
                            .unwrap()
                    },
                ),
            );
        let server = ServerHandle::bind(LOOPBACK, router, Arc::clone(&admission))
            .await
            .unwrap();
        let mut client = RawClient::request(server.local_addr(),
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\nx").await;
        assert!(client.head.starts_with("HTTP/1.1 101"));
        let mut session_ready = [0; 5];
        timeout(DEADLINE, client.io.read_exact(&mut session_ready))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&session_ready, b"ready");
        assert_eq!(admission.active_requests(), 0);
        assert_eq!(admission.active_websockets(), 1);
        assert!(admission.acquire_request().is_some());
        assert!(admission.acquire_websocket().is_none());

        let address = server.local_addr();
        let shutdown = server.shutdown_signal();
        let mut stopping = pin!(server.shutdown());
        assert!(poll_once(stopping.as_mut()).await.is_pending());
        let mut session_stopping = [0; 8];
        timeout(DEADLINE, client.io.read_exact(&mut session_stopping))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&session_stopping, b"stopping");
        assert_eq!(shutdown.wait().await, ShutdownReason::Requested);
        assert!(poll_once(stopping.as_mut()).await.is_pending());
        assert_eq!(admission.active_requests(), 0);
        assert_eq!(admission.active_websockets(), 1);
        client.io.write_all(b"y").await.unwrap();
        assert!(client.rest().await.is_empty());
        timeout(DEADLINE, stopping).await.unwrap().unwrap();
        assert!(TcpStream::connect(address).await.is_err());
        assert_eq!(admission.active_requests(), 0);
        assert_eq!(admission.active_websockets(), 0);
    }
}

#[tokio::test]
async fn exit_signals_after_socket_write_and_distinguishes_failed_write() {
    for expected in [
        ShutdownReason::ResponseWritten,
        ShutdownReason::ResponseWriteFailed,
    ] {
        let control = Arc::new(ServerControl::new(Arc::new(ConnectionAdmission::new(
            ConnectionAdmissionConfig::default(),
        ))));
        let router = Router::new().route(
            "/",
            get(|| async { (Extension(ShutdownAfterResponse), "stopping") }),
        );
        let (entered, attempting_write) = oneshot::channel();
        let (release, wait) = oneshot::channel();
        let (mut peer, io) = duplex(1024);
        peer.write_all(GET).await.unwrap();
        let permit = control.admission.acquire_request().unwrap();
        let driver = tokio::spawn(drive(
            PausedWrites::new(
                io,
                Some(WriteLatch {
                    entered: Some(entered),
                    release: wait,
                }),
            ),
            LOOPBACK,
            Instant::now(),
            router,
            Arc::clone(&control),
            permit,
        ));

        timeout(DEADLINE, attempting_write).await.unwrap().unwrap();
        let mut shutdown = pin!(control.signal().wait());
        assert!(poll_once(shutdown.as_mut()).await.is_pending());
        assert_eq!(control.admission.active_requests(), 1);

        let (mut other, io) = duplex(1024);
        other.write_all(GET).await.unwrap();
        drive(
            io,
            LOOPBACK,
            Instant::now(),
            Router::new(),
            Arc::clone(&control),
            control.admission.acquire_request().unwrap(),
        )
        .await;
        assert!(poll_once(shutdown.as_mut()).await.is_pending());

        if expected == ShutdownReason::ResponseWritten {
            release.send(()).unwrap();
        } else {
            drop(release);
        }
        assert_eq!(timeout(DEADLINE, shutdown).await.unwrap(), expected);
        driver.await.unwrap();
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        if expected == ShutdownReason::ResponseWritten {
            assert!(bytes.starts_with(b"HTTP/1.1 200 OK\r\n"));
            assert!(bytes.ends_with(b"\r\n\r\nstopping"));
        } else {
            assert!(bytes.is_empty());
        }
        assert_eq!(control.admission.active_requests(), 0);
        assert!(control.admission.acquire_request().is_none());
    }
}

#[tokio::test]
async fn wire_guard_returns_legacy_400_status_and_body() {
    const INVALID_HEAD: &[u8] = br#"{"error":{"message":"invalid HTTP request"}}"#;
    const INVALID_FRAMING: &[u8] =
        br#"{"error":{"message":"duplicate Content-Length or unsupported Transfer-Encoding","origin":"emp"}}"#;
    let router = Router::new().fallback(|Extension(raw): Extension<RawRequestHead>| async move {
        raw.request()
            .validate_body_framing()
            .map_err(http::error::EntryError::from)?;
        Ok::<_, http::error::EntryError>("unexpected route")
    });
    let server = ServerHandle::bind(LOOPBACK, router, admission())
        .await
        .unwrap();
    let oversized = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "x".repeat(8192));
    for (request, expected_body) in [
        (b"POST / HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\ncOnTeNt-LeNgTh: 0\r\n\r\n".as_slice(), INVALID_FRAMING),
        (b"POST / HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".as_slice(), INVALID_FRAMING),
        (b"POST / HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: identity\r\ntransfer-encoding: identity\r\n\r\n".as_slice(), INVALID_FRAMING),
        (b"GET / HTTP/1.0\r\n\r\n".as_slice(), INVALID_HEAD),
        (b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".as_slice(), INVALID_HEAD),
        (b"PUT / HTTP/1.1\r\n\r\n".as_slice(), INVALID_HEAD),
        (oversized.as_bytes(), INVALID_HEAD),
    ] {
        let mut client = RawClient::request(server.local_addr(), request).await;
        assert!(client.head.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{}", client.head);
        assert_eq!(client.rest().await, expected_body);
    }
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn exact_header_boundary_allows_over_100_fields_and_only_one_response() {
    let router = Router::new()
        .route(
            "/",
            get(
                |Extension(raw): Extension<RawRequestHead>,
                 ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                    assert!(peer.ip().is_loopback());
                    assert_eq!(raw.0.len(), 8196);
                    assert_eq!(raw.0.matches("X:\r\n").count(), 150);
                    "ready"
                },
            ),
        )
        .route("/again", get(|| async { "second response" }));
    let server = ServerHandle::bind(LOOPBACK, router, admission())
        .await
        .unwrap();
    let mut pipeline = format!(
        "GET  /  HTTP/1.1\r\nHost: localhost\r\n{}X-Pad: ",
        "X:\r\n".repeat(150)
    );
    pipeline.push_str(&"x".repeat(8192 - pipeline.len()));
    pipeline.push_str("\r\n\r\nGET /again HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let mut client = RawClient::request(server.local_addr(), pipeline.as_bytes()).await;
    assert!(client.head.starts_with("HTTP/1.1 200"));
    assert_eq!(client.rest().await, b"ready");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn synchronous_service_work_runs_off_the_runtime_worker() {
    let runtime_worker = std::thread::current().id();
    let (entered, waiting) = oneshot::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    let work = tokio::spawn(run_blocking(move || {
        entered.send(std::thread::current().id()).unwrap();
        resumed.recv_timeout(DEADLINE).unwrap();
        42
    }));
    assert_ne!(
        timeout(DEADLINE, waiting).await.unwrap().unwrap(),
        runtime_worker
    );
    resume.send(()).unwrap();
    assert_eq!(timeout(DEADLINE, work).await.unwrap().unwrap().unwrap(), 42);
}

struct WriteLatch {
    entered: Option<oneshot::Sender<()>>,
    release: oneshot::Receiver<()>,
}

struct PausedWrites<I> {
    io: I,
    latch: Option<WriteLatch>,
}

impl<I> PausedWrites<I> {
    fn new(io: I, latch: Option<WriteLatch>) -> Self {
        Self { io, latch }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for PausedWrites<I> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buffer)
    }
}

impl<I: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for PausedWrites<I> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        use std::{future::Future, pin::Pin, task::Poll};
        if let Some(latch) = self.latch.as_mut() {
            if let Some(entered) = latch.entered.take() {
                let _ = entered.send(());
            }
            match Pin::new(&mut latch.release).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(_)) => return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
                Poll::Ready(Ok(())) => self.latch = None,
            }
        }
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
