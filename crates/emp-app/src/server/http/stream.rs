use crate::{
    http::response::status_text, server::http::error::response_head,
    services::connection_admission::ConnectionPermit,
};
use axum::{
    body::{Body, Bytes},
    http::response::Builder,
    response::Response,
};
use hyper::body::{Body as HttpBody, Frame};
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
}

impl BodyOutput {
    pub(super) fn new(
        socket: Arc<TcpStream>,
        delivery: Arc<Delivery>,
        response: oneshot::Sender<Response>,
    ) -> Self {
        Self {
            socket,
            delivery,
            response: Some(response),
            stream: None,
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

    pub(crate) fn frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send_frame(Bytes::copy_from_slice(bytes), None)
    }

    pub(super) fn terminal_frame(
        &mut self,
        bytes: Vec<u8>,
        business: Option<ConnectionPermit>,
    ) -> io::Result<()> {
        self.send_frame(Bytes::from(bytes), business)
    }

    fn send_frame(&mut self, bytes: Bytes, business: Option<ConnectionPermit>) -> io::Result<()> {
        let stream = self.stream.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        stream
            .send
            .blocking_send(bytes)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        drop(business);
        stream
            .confirmation
            .blocking_recv()
            .ok_or(io::ErrorKind::BrokenPipe)?;
        Ok(())
    }

    pub(super) fn finish(self, response: impl FnOnce() -> Response) {
        if let Some(sender) = self.response {
            let _ = sender.send(response());
        }
    }
}

pub(super) struct StreamFrames {
    send: mpsc::Sender<Bytes>,
    confirmation: mpsc::Receiver<()>,
}

impl StreamFrames {
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
