use crate::server::{handoff_response, test_hooks::ServerHooks};
use crate::{
    http::response::status_text,
    server::http::error::response_head,
    services::{
        connection_admission::BusinessToken,
        events::{sse_frame, terminal_event_type},
    },
};
use axum::{
    body::{Body, Bytes},
    http::response::Builder,
    response::Response,
};
use emp_transport::SseJsonParser;
use hyper::body::{Body as HttpBody, Frame};
use serde_json::{Map, Value, json};
use std::{
    convert::Infallible,
    io,
    net::TcpStream,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};

#[derive(Default)]
pub(in crate::server) struct StreamConnection {
    pub(in crate::server) delivery: Arc<Delivery>,
    pub(in crate::server) write_timed_out: Notify,
    pub(in crate::server) worker: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Default)]
pub(in crate::server) struct Delivery {
    pending: Mutex<Option<Flushed>>,
    bytes: AtomicUsize,
}

#[derive(Clone)]
pub(in crate::server) struct Flushed(mpsc::Sender<()>);

impl Delivery {
    pub(in crate::server) fn await_flush(&self, flushed: Flushed) {
        let previous = self.pending.lock().expect("HTTP flush").replace(flushed);
        assert!(previous.is_none());
    }

    pub(in crate::server) fn written(&self, count: usize) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    pub(in crate::server) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    pub(in crate::server) fn flushed(&self) {
        if let Some(sender) = self.pending.lock().expect("HTTP flush").take() {
            let _ = sender.0.try_send(());
        }
    }

    pub(in crate::server) fn close(&self) {
        self.pending.lock().expect("HTTP flush").take();
    }
}

pub(crate) struct BodyOutput {
    socket: Arc<TcpStream>,
    delivery: Arc<Delivery>,
    response: Option<oneshot::Sender<Response>>,
    stream: Option<StreamFrames>,
    business: Option<BusinessToken>,
    hooks: Arc<ServerHooks>,
}

impl BodyOutput {
    pub(super) fn new(
        socket: Arc<TcpStream>,
        delivery: Arc<Delivery>,
        response: oneshot::Sender<Response>,
        business: BusinessToken,
        hooks: Arc<ServerHooks>,
    ) -> Self {
        Self {
            socket,
            delivery,
            response: Some(response),
            stream: None,
            business: Some(business),
            hooks,
        }
    }

    pub(crate) fn socket(&self) -> &TcpStream {
        &self.socket
    }

    pub(crate) fn head<'a>(
        &mut self,
        status: u16,
        content_type: &str,
        headers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let builder = response_head(status, content_type)
            .header("cache-control", "no-cache")
            .extension(hyper::ext::ReasonPhrase::from_static(
                status_text(status).as_bytes(),
            ));
        let builder = headers.into_iter().fold(builder, |builder, (name, value)| {
            builder.header(name, value)
        });
        let (response, mut stream) = StreamFrames::start(builder, self.delivery.clone());
        self.response
            .take()
            .expect("response head is sent once")
            .send(response)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        stream
            .confirmation
            .blocking_recv()
            .ok_or(io::ErrorKind::BrokenPipe)?;
        self.stream = Some(stream);
        Ok(())
    }

    pub(crate) fn response_event(&mut self, kind: &str, frames: &[Vec<u8>]) -> io::Result<()> {
        if let Some((last, preceding)) = frames.split_last() {
            for frame in preceding {
                self.write_frame(Bytes::copy_from_slice(frame), false)?;
            }
            self.write_frame(Bytes::copy_from_slice(last), terminal_event_type(kind))?;
        }
        Ok(())
    }

    pub(crate) fn aero_chunk(
        &mut self,
        chunk: &[u8],
        parser: Option<&mut SseJsonParser>,
    ) -> io::Result<Vec<Map<String, Value>>> {
        let events = parser
            .and_then(|parser| parser.push(chunk).ok())
            .unwrap_or_default();
        let terminal = events.last().is_some_and(|event| {
            event
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(terminal_event_type)
        });
        self.write_frame(Bytes::copy_from_slice(chunk), terminal)?;
        Ok(events)
    }

    pub(super) fn report_progress(&mut self, completed: i64, total: i64) -> io::Result<()> {
        let frame = sse_frame("progress", &json!({"completed":completed,"total":total}))?;
        self.write_frame(Bytes::from(frame), false)
    }

    pub(super) fn finish_report(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        self.write_frame(Bytes::from(bytes), true)
    }

    fn write_frame(&mut self, bytes: Bytes, terminal: bool) -> io::Result<()> {
        let stream = self.stream.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        if terminal {
            let permit = tokio::runtime::Handle::current()
                .block_on(stream.send.reserve())
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
            handoff_response(self.business.take(), || {
                permit.send(bytes);
                self.hooks.output_queued();
            });
        } else {
            stream
                .send
                .blocking_send(bytes)
                .map_err(|_| io::ErrorKind::BrokenPipe)?;
        }
        stream.wait_for_flush()
    }

    pub(super) fn finish(self, response: impl FnOnce() -> Response) {
        let reply = self.response.map(|sender| (sender, response()));
        handoff_response(self.business, || {
            if let Some((sender, response)) = reply {
                let _ = sender.send(response);
                self.hooks.output_queued();
            }
            drop(self.stream);
        });
    }
}

pub(super) struct StreamFrames {
    send: mpsc::Sender<Bytes>,
    confirmation: mpsc::Receiver<()>,
}

impl StreamFrames {
    fn wait_for_flush(&mut self) -> io::Result<()> {
        self.confirmation
            .blocking_recv()
            .ok_or(io::ErrorKind::BrokenPipe.into())
    }

    pub(super) fn start(builder: Builder, delivery: Arc<Delivery>) -> (Response, Self) {
        let (send, frames) = mpsc::channel(1);
        let (flushed, confirmation) = mpsc::channel(1);
        let flushed = Flushed(flushed);
        let response = builder
            .extension(flushed.clone())
            .body(Body::new(EventBody {
                frames,
                flushed,
                delivery,
            }))
            .expect("EMP streaming response");
        (response, Self { send, confirmation })
    }

    pub(super) async fn flushed(&mut self) -> io::Result<()> {
        self.confirmation
            .recv()
            .await
            .ok_or(io::ErrorKind::BrokenPipe.into())
    }

    pub(super) async fn frame(&mut self, bytes: Vec<u8>) -> io::Result<()> {
        self.send
            .send(Bytes::from(bytes))
            .await
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.flushed().await
    }

    pub(super) async fn closed(&self) {
        self.send.closed().await;
    }
}

struct EventBody {
    frames: mpsc::Receiver<Bytes>,
    flushed: Flushed,
    delivery: Arc<Delivery>,
}

impl HttpBody for EventBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        self.frames.poll_recv(cx).map(|frame| {
            frame.map(|bytes| {
                self.delivery.await_flush(self.flushed.clone());
                Ok(Frame::data(bytes))
            })
        })
    }
}
