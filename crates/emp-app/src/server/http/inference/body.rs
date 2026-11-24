use crate::{http::response::status_text, server::http::error::response_head};
use axum::{
    body::{Body, Bytes},
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
use tokio::sync::{mpsc, oneshot};

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
    stream: Option<(mpsc::Sender<Bytes>, mpsc::Receiver<()>)>,
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
        let (send, frames) = mpsc::channel(1);
        let (flushed, mut confirmation) = mpsc::channel(1);
        let flushed = Flushed(flushed);
        let builder = response_head(status, content_type)
            .header("cache-control", "no-cache")
            .extension(hyper::ext::ReasonPhrase::from_static(
                status_text(status).as_bytes(),
            ))
            .extension(flushed.clone());
        let response = headers
            .into_iter()
            .fold(builder, |builder, (name, value)| {
                builder.header(name, value)
            })
            .body(Body::new(EventBody {
                frames,
                flushed,
                delivery: self.delivery.clone(),
            }))
            .expect("EMP streaming response");
        self.response
            .take()
            .expect("response head is sent once")
            .send(response)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        confirmation
            .blocking_recv()
            .ok_or(io::ErrorKind::BrokenPipe)?;
        self.stream = Some((send, confirmation));
        Ok(())
    }

    pub(crate) fn frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        let (send, confirmation) = self.stream.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        send.blocking_send(Bytes::copy_from_slice(bytes))
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        confirmation
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
