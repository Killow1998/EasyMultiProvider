#[cfg(test)]
use crate::server::test_hooks::{ServerHooks, WriteHook};
use crate::{
    http::response::status_text,
    server::http::error::response_head,
    services::{
        connection_admission::BusinessToken,
        gateway_clients::Caller,
        modules::aero::{Call, CallLease, CallParent, Callers},
    },
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
    task::{Context, Poll, Waker, ready},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub(in crate::server) struct StreamConnection {
    pub(in crate::server) delivery: Arc<Delivery>,
    pub(in crate::server) worker: Mutex<Option<JoinHandle<()>>>,
}

impl StreamConnection {
    pub(in crate::server) fn new(#[cfg(test)] write_hook: WriteHook) -> Self {
        Self {
            delivery: Arc::new(Delivery {
                #[cfg(test)]
                write_hook,
                ..Delivery::default()
            }),
            worker: Mutex::default(),
        }
    }
}

#[derive(Default)]
pub(in crate::server) struct Delivery {
    pending: Mutex<PendingFlush>,
    bytes: AtomicUsize,
    #[cfg(test)]
    write_hook: WriteHook,
}

#[derive(Default)]
struct PendingFlush {
    ack: Option<Flushed>,
    body: Option<Waker>,
}

#[derive(Clone)]
pub(in crate::server) struct Flushed(mpsc::Sender<()>);

impl Delivery {
    pub(in crate::server) fn await_flush(&self, flushed: Flushed) {
        let previous = self
            .pending
            .lock()
            .expect("HTTP flush")
            .ack
            .replace(flushed);
        assert!(previous.is_none());
    }

    pub(in crate::server) fn written(&self, count: usize) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }

    pub(in crate::server) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    pub(in crate::server) fn flushed(&self) {
        let PendingFlush { ack, body } =
            std::mem::take(&mut *self.pending.lock().expect("HTTP flush"));
        if let Some(sender) = ack {
            let _ = sender.0.try_send(());
        }
        if let Some(body) = body {
            body.wake();
        }
    }

    pub(in crate::server) fn close(&self) {
        std::mem::take(&mut *self.pending.lock().expect("HTTP flush"));
    }

    fn poll_flushed(&self, cx: &Context<'_>) -> Poll<()> {
        let mut pending = self.pending.lock().expect("HTTP flush");
        if pending.ack.is_some() {
            pending.body = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

pub(crate) struct BodyOutput {
    socket: Arc<TcpStream>,
    delivery: Arc<Delivery>,
    response: Option<oneshot::Sender<Response>>,
    stream: Option<StreamFrames>,
    work: Option<ResponseWork>,
    frame_pending: bool,
    #[cfg(test)]
    hooks: Arc<ServerHooks>,
}

enum ResponseWork {
    Request(BusinessToken),
    Aero(Call),
}

impl ResponseWork {
    fn handoff(work: Option<Self>, publish: impl FnOnce()) {
        match work {
            Some(Self::Aero(mut call)) => call.finish_with(publish),
            Some(Self::Request(business)) => business.finish_with(publish),
            None => publish(),
        }
    }
}

impl BodyOutput {
    pub(super) fn new(
        socket: Arc<TcpStream>,
        delivery: Arc<Delivery>,
        response: oneshot::Sender<Response>,
        business: BusinessToken,
        #[cfg(test)] hooks: Arc<ServerHooks>,
    ) -> Self {
        Self {
            socket,
            delivery,
            response: Some(response),
            stream: None,
            work: Some(ResponseWork::Request(business)),
            frame_pending: false,
            #[cfg(test)]
            hooks,
        }
    }

    pub(crate) fn socket(&self) -> &TcpStream {
        &self.socket
    }

    pub(crate) fn delegate(
        &mut self,
        callers: &Callers,
        caller: Caller,
        authorization: Option<String>,
        lease: Option<CallLease>,
    ) -> Option<String> {
        let call = callers.delegate(caller, authorization, lease, || {
            let Some(ResponseWork::Request(business)) = self.work.take() else {
                unreachable!("Aero delegates an active inference request once");
            };
            CallParent::Request(business)
        })?;
        let key = call.key.clone();
        self.work = Some(ResponseWork::Aero(call));
        Some(key)
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
        let (response, stream) = StreamFrames::start(builder, self.delivery.clone());
        self.stream = Some(stream);
        self.response
            .take()
            .expect("response head is sent once")
            .send(response)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.stream
            .as_mut()
            .expect("response stream")
            .wait_for_flush()
    }

    pub(crate) fn write_frame(&mut self, bytes: Bytes) -> io::Result<()> {
        self.flush_frame()?;
        let stream = self.stream.as_mut().expect("response stream");
        #[cfg(test)]
        let queued = self.delivery.write_hook.pause_for_frame(&bytes);
        #[cfg(test)]
        let observed = queued.as_ref().map(|_| bytes.to_vec());
        stream
            .send
            .blocking_send(bytes)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        self.frame_pending = true;
        #[cfg(test)]
        if let Some(queued) = queued {
            let _ = queued.send(observed);
        }
        Ok(())
    }

    pub(crate) fn flush_frame(&mut self) -> io::Result<()> {
        if std::mem::take(&mut self.frame_pending) {
            self.stream
                .as_mut()
                .expect("response stream")
                .wait_for_flush()?;
        }
        Ok(())
    }

    pub(super) fn finish(mut self, response: impl FnOnce() -> Response) -> io::Result<()> {
        let reply = self.response.is_some().then(response);
        self.finish_output(reply)
    }

    pub(crate) fn finish_producing(&mut self) -> io::Result<()> {
        if matches!(self.work, Some(ResponseWork::Aero(_))) {
            return self.finish_output(None);
        }
        ResponseWork::handoff(self.work.take(), || {
            #[cfg(test)]
            self.hooks.stream_handoff();
        });
        self.flush_frame()
    }

    fn finish_output(&mut self, reply: Option<Response>) -> io::Result<()> {
        let confirmation = self.close(reply);
        if std::mem::take(&mut self.frame_pending) {
            confirmation
                .expect("queued response frame")
                .blocking_recv()
                .ok_or(io::ErrorKind::BrokenPipe)?;
        }
        Ok(())
    }

    fn close(&mut self, reply: Option<Response>) -> Option<mpsc::Receiver<()>> {
        let (send, confirmation) = self
            .stream
            .take()
            .map(|stream| (stream.send, stream.confirmation))
            .unzip();
        let sender = self.response.take();
        let work = self.work.take();
        #[cfg(test)]
        let stream_handoff = send.is_some() && work.is_some();
        ResponseWork::handoff(work, || {
            if let (Some(sender), Some(response)) = (sender, reply) {
                let publish = || {
                    let _ = sender.send(response);
                };
                #[cfg(test)]
                let publish = || {
                    self.hooks
                        .publish_output(Some(&self.delivery.write_hook), publish)
                };
                publish();
            }
            #[cfg(test)]
            if stream_handoff {
                self.hooks.stream_handoff();
            }
            drop(send);
        });
        confirmation
    }
}

impl Drop for BodyOutput {
    fn drop(&mut self) {
        drop(self.close(None));
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
        ready!(self.delivery.poll_flushed(cx));
        self.frames.poll_recv(cx).map(|frame| {
            frame.map(|bytes| {
                self.delivery.await_flush(self.flushed.clone());
                Ok(Frame::data(bytes))
            })
        })
    }
}
