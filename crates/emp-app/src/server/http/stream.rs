use crate::server::test_hooks::{ServerHooks, WriteHook};
use crate::{
    http::response::status_text,
    server::http::error::response_head,
    services::{
        connection_admission::BusinessToken,
        events::{SseFrame, TerminalEvent, sse_frame},
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
use serde_json::json;
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
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub(in crate::server) struct StreamConnection {
    pub(in crate::server) delivery: Arc<Delivery>,
    pub(in crate::server) worker: Mutex<Option<JoinHandle<()>>>,
}

impl StreamConnection {
    pub(in crate::server) fn new(write_hook: WriteHook) -> Self {
        Self {
            delivery: Arc::new(Delivery {
                write_hook,
                ..Delivery::default()
            }),
            worker: Mutex::default(),
        }
    }
}

#[derive(Default)]
pub(in crate::server) struct Delivery {
    pending: Mutex<Option<Flushed>>,
    bytes: AtomicUsize,
    write_hook: WriteHook,
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
    work: Option<ResponseWork>,
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
        hooks: Arc<ServerHooks>,
    ) -> Self {
        Self {
            socket,
            delivery,
            response: Some(response),
            stream: None,
            work: Some(ResponseWork::Request(business)),
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

    pub(crate) fn response_event(
        &mut self,
        frames: &[Vec<u8>],
    ) -> (io::Result<()>, Option<TerminalEvent>) {
        let mut terminal = None;
        for frame in frames {
            let (result, event) = self.write_sse_frame(Bytes::copy_from_slice(frame), false);
            terminal = event.or(terminal);
            if result.is_err() {
                return (result, terminal);
            }
        }
        (Ok(()), terminal)
    }

    pub(super) fn report_terminal(&mut self, frame: Vec<u8>) -> io::Result<()> {
        self.write_sse_frame(Bytes::from(frame), true).0
    }

    fn write_sse_frame(
        &mut self,
        bytes: Bytes,
        report: bool,
    ) -> (io::Result<()>, Option<TerminalEvent>) {
        let mut terminal = None;
        SseFrame::new(false).push(&bytes, |event, _| {
            terminal = event
                .filter(|event| report || event.response_outcome().is_some())
                .or(terminal);
        });
        let result = if terminal.is_some() {
            self.finish_frame(bytes)
        } else {
            self.write_frame(bytes)
        };
        (result, terminal)
    }

    pub(super) fn report_progress(&mut self, completed: i64, total: i64) -> io::Result<()> {
        let frame = sse_frame("progress", &json!({"completed":completed,"total":total}))?;
        self.write_frame(Bytes::from(frame))
    }

    pub(crate) fn write_frame(&mut self, bytes: Bytes) -> io::Result<()> {
        let stream = self.stream.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        stream
            .send
            .blocking_send(bytes)
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        stream.wait_for_flush()
    }

    pub(crate) fn finish_frame(&mut self, bytes: Bytes) -> io::Result<()> {
        let stream = self.stream.as_mut().ok_or(io::ErrorKind::BrokenPipe)?;
        let permit = tokio::runtime::Handle::current()
            .block_on(stream.send.reserve())
            .map_err(|_| io::ErrorKind::BrokenPipe)?;
        ResponseWork::handoff(self.work.take(), || {
            self.hooks
                .publish_output(Some(&self.delivery.write_hook), Some(&bytes), || {
                    permit.send(bytes.clone());
                });
        });
        stream.wait_for_flush()
    }

    pub(super) fn finish(mut self, response: impl FnOnce() -> Response) {
        let reply = self.response.is_some().then(response);
        self.close(reply);
    }

    fn close(&mut self, reply: Option<Response>) {
        let stream = self.stream.take();
        let sender = self.response.take();
        let work = self.work.take();
        let aero_eof = matches!(work, Some(ResponseWork::Aero(_))) && stream.is_some();
        ResponseWork::handoff(work, || {
            if let (Some(sender), Some(response)) = (sender, reply) {
                self.hooks
                    .publish_output(Some(&self.delivery.write_hook), None, || {
                        let _ = sender.send(response);
                    });
            }
            if aero_eof {
                self.hooks
                    .publish_output(Some(&self.delivery.write_hook), None, || drop(stream));
            } else {
                drop(stream);
            }
        });
    }
}

impl Drop for BodyOutput {
    fn drop(&mut self) {
        self.close(None);
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
