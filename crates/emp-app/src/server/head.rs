use super::ServerMode;
use crate::http::request::{
    REQUEST_READ_TIMEOUT, REQUEST_VERSION, Request, parse_request, request_head_end,
    reserved_source_header,
};
use crate::server::http::{error::EntryError, stream::Delivery};
use axum::body::Bytes;
use std::{
    io::{self, Cursor},
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll, Waker, ready},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf},
    time::{Duration, Instant, Sleep, sleep, timeout_at},
};

#[derive(Clone)]
pub(crate) struct RawRequestHead(pub(crate) Arc<str>, usize, Arc<Mutex<BodyRead>>);

impl RawRequestHead {
    pub(crate) fn request(&self) -> Request<'_> {
        parse_request(&self.0).expect("the connection already parsed the request head")
    }

    pub(crate) fn body_prefix_len(&self) -> usize {
        self.1
    }

    pub(crate) fn allow_body(&self) {
        let previous = std::mem::replace(
            &mut *self.2.lock().expect("request body gate"),
            BodyRead::Open,
        );
        if let BodyRead::Waiting(Some(waker)) = previous {
            waker.wake();
        }
    }
}

enum BodyRead {
    Waiting(Option<Waker>),
    Open,
}

pub(super) struct GuardedHead {
    pub(super) raw: RawRequestHead,
    pub(super) release_request_permit: bool,
    body_prefix: Vec<u8>,
}

impl GuardedHead {
    pub(super) async fn read(
        io: &mut (impl AsyncRead + Unpin),
        deadline: Instant,
    ) -> Result<Self, EntryError> {
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(EntryError::InvalidHead);
            }
            let until = deadline.min(now + REQUEST_READ_TIMEOUT);
            let count = timeout_at(until, io.read(&mut buffer))
                .await
                .map_err(|_| EntryError::InvalidHead)?
                .map_err(|_| EntryError::InvalidHead)?;
            if count == 0 || Instant::now() >= deadline {
                return Err(EntryError::InvalidHead);
            }
            let previous_len = bytes.len();
            bytes.extend_from_slice(&buffer[..count]);
            let Some(end) =
                request_head_end(&bytes, previous_len).map_err(|()| EntryError::InvalidHead)?
            else {
                continue;
            };
            let body_prefix = bytes.split_off(end);
            let head = str::from_utf8(&bytes).map_err(|_| EntryError::InvalidHead)?;
            let request = parse_request(head).ok_or(EntryError::InvalidHead)?;
            return Ok(Self {
                raw: RawRequestHead(
                    Arc::from(head),
                    body_prefix.len(),
                    Arc::new(Mutex::new(BodyRead::Waiting(None))),
                ),
                release_request_permit: request.releases_request_permit(),
                body_prefix,
            });
        }
    }

    pub(super) fn replay<I>(
        self,
        io: I,
        transport_target: &str,
        response: Arc<OnceLock<Option<Bytes>>>,
        mode: ServerMode,
        delivery: Option<Arc<Delivery>>,
    ) -> Prefixed<I> {
        let request = self.raw.request();
        let upgrade = crate::server::http::websocket::is_upgrade_route(request);
        let transport_length = if !upgrade
            && request.header_count("Content-Length") == 1
            && request.header("Transfer-Encoding").is_none()
        {
            request
                .header("Content-Length")
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|&length| (length as u64) < u64::MAX - 1)
        } else {
            None
        };
        if upgrade
            || request.header_count("Content-Length") == 0
                && request.header("Transfer-Encoding").is_none()
            || transport_length == Some(0)
        {
            self.raw.allow_body();
        }
        let mut head = format!(
            "{} {transport_target} {REQUEST_VERSION}\r\n",
            request.method.as_str()
        )
        .into_bytes();
        if upgrade {
            head.extend_from_slice(b"Connection: Upgrade\r\nUpgrade: websocket\r\n");
        } else {
            for line in self.raw.0.split_inclusive('\n').skip(1) {
                let Some((name, _)) = line.split_once(':') else {
                    if !line.trim().is_empty() {
                        head.extend_from_slice(line.as_bytes());
                    }
                    continue;
                };
                let name = name.trim();
                if name.eq_ignore_ascii_case("Content-Length") {
                    if let Some(length) = transport_length {
                        use std::io::Write;
                        let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
                        write!(head, "Content-Length:{length}{ending}").expect("write into Vec");
                    }
                } else if !name.eq_ignore_ascii_case("Transfer-Encoding")
                    && !name.eq_ignore_ascii_case("Expect")
                    && !reserved_source_header(name)
                {
                    head.extend_from_slice(line.as_bytes());
                }
            }
        }
        head.extend_from_slice(b"\r\n");
        Prefixed {
            io,
            head: Cursor::new(head),
            prefix: Cursor::new(self.body_prefix),
            body: self.raw.2,
            response,
            mode,
            written: 0,
            recovery_write_deadline: None,
            delivery,
        }
    }
}

pub(super) struct Prefixed<I> {
    io: I,
    head: Cursor<Vec<u8>>,
    prefix: Cursor<Vec<u8>>,
    body: Arc<Mutex<BodyRead>>,
    response: Arc<OnceLock<Option<Bytes>>>,
    mode: ServerMode,
    written: usize,
    recovery_write_deadline: Option<Pin<Box<Sleep>>>,
    delivery: Option<Arc<Delivery>>,
}

impl<I> Drop for Prefixed<I> {
    fn drop(&mut self) {
        if let Some(delivery) = &self.delivery {
            delivery.close();
        }
    }
}

impl<I: AsyncRead + Unpin> AsyncRead for Prefixed<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if read_prefix(&mut self.head, output) {
            return Poll::Ready(Ok(()));
        }
        {
            let mut body = self.body.lock().expect("request body gate");
            if let BodyRead::Waiting(waker) = &mut *body {
                *waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
        }
        if read_prefix(&mut self.prefix, output) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, output)
    }
}

fn read_prefix(prefix: &mut Cursor<Vec<u8>>, output: &mut ReadBuf<'_>) -> bool {
    let offset = prefix.position() as usize;
    let bytes = &prefix.get_ref()[offset..];
    if bytes.is_empty() {
        return false;
    }
    let count = output.remaining().min(bytes.len());
    output.put_slice(&bytes[..count]);
    prefix.set_position((offset + count) as u64);
    true
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Prefixed<I> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let response = this.response.get_or_init(|| {
            Some(match this.mode {
                ServerMode::Http => Bytes::from(EntryError::InvalidHead.reply().encode()),
                ServerMode::Recovery => Bytes::new(),
            })
        });
        if let Some(response) = response {
            while this.written < response.len() {
                if matches!(this.mode, ServerMode::Recovery) {
                    let deadline = this
                        .recovery_write_deadline
                        .get_or_insert_with(|| Box::pin(sleep(Duration::from_secs(3))));
                    if deadline.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
                    }
                }
                let count =
                    ready!(Pin::new(&mut this.io).poll_write(cx, &response[this.written..]))?;
                this.recovery_write_deadline = None;
                if count == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                this.written += count;
            }
            return Poll::Ready(Ok(bytes.len()));
        }
        let count = ready!(Pin::new(&mut this.io).poll_write(cx, bytes))?;
        if let Some(delivery) = &this.delivery {
            delivery.written(count);
        }
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(Pin::new(&mut self.io).poll_flush(cx))?;
        if let Some(delivery) = &self.delivery {
            delivery.flushed();
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
