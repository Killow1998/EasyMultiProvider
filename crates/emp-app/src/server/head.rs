use super::ServerMode;
use crate::http::request::{
    REQUEST_READ_TIMEOUT, REQUEST_VERSION, Request, RequestMethod, RequestMethods,
    parse_request_with_methods, request_head_end, reserved_source_header,
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
pub(crate) struct RawRequestHead(
    pub(crate) Arc<str>,
    usize,
    Arc<Mutex<BodyRead>>,
    RequestMethods,
);

impl RawRequestHead {
    pub(crate) fn request(&self) -> Request<'_> {
        parse_request_with_methods(&self.0, self.3)
            .expect("the connection already parsed the request head")
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
    pub(super) account_events: bool,
    body_prefix: Vec<u8>,
}

impl GuardedHead {
    pub(super) async fn read(
        io: &mut (impl AsyncRead + Unpin),
        deadline: Instant,
        methods: RequestMethods,
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
            let request =
                parse_request_with_methods(head, methods).ok_or(EntryError::InvalidHead)?;
            return Ok(Self {
                raw: RawRequestHead(
                    Arc::from(head),
                    body_prefix.len(),
                    Arc::new(Mutex::new(BodyRead::Waiting(None))),
                    methods,
                ),
                account_events: request.method == RequestMethod::Get
                    && request.raw_path() == "/api/accounts/events",
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
            stream_deadline: self.account_events.then(WriteDeadline::default),
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
    stream_deadline: Option<WriteDeadline>,
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
        let response = this
            .response
            .get_or_init(|| Some(Bytes::from(EntryError::InvalidHead.reply().encode())));
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
        let mut write = |cx: &mut Context<'_>| Pin::new(&mut this.io).poll_write(cx, bytes);
        let count = ready!(match &mut this.stream_deadline {
            Some(deadline) => deadline.poll(cx, write, |&count| count > 0),
            None => write(cx),
        })?;
        if let Some(delivery) = &this.delivery {
            delivery.written(count);
        }
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut flush = |cx: &mut Context<'_>| Pin::new(&mut this.io).poll_flush(cx);
        ready!(match &mut this.stream_deadline {
            Some(deadline) => deadline.poll(cx, flush, |()| true),
            None => flush(cx),
        })?;
        if let Some(delivery) = &this.delivery {
            delivery.flushed();
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[derive(Default)]
struct WriteDeadline(Option<Pin<Box<Sleep>>>);

impl WriteDeadline {
    fn poll<T>(
        &mut self,
        cx: &mut Context<'_>,
        io: impl FnOnce(&mut Context<'_>) -> Poll<io::Result<T>>,
        progressed: impl FnOnce(&T) -> bool,
    ) -> Poll<io::Result<T>> {
        if self
            .0
            .as_ref()
            .is_some_and(|timer| Instant::now() >= timer.deadline())
        {
            return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
        }
        let result = io(cx);
        match &result {
            Poll::Ready(Ok(value)) if progressed(value) => self.0 = None,
            Poll::Pending => {
                let deadline = self
                    .0
                    .get_or_insert_with(|| Box::pin(sleep(Duration::from_secs(5))));
                if deadline.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
                }
            }
            _ => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tests::poll_once;
    use emp_test_support::test_timeout;
    use std::pin::pin;
    use tokio::{
        io::{AsyncWriteExt, duplex},
        time::{advance, pause},
    };

    async fn account_event_stream<I>(io: I) -> Prefixed<CountingIo<I>> {
        let request = b"GET /api/accounts/events HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let head = GuardedHead::read(
            &mut request.as_slice(),
            Instant::now() + test_timeout(REQUEST_READ_TIMEOUT).duration(),
            RequestMethods::Standard,
        )
        .await
        .ok()
        .unwrap();
        head.replay(
            CountingIo {
                inner: io,
                calls: [0; 2],
            },
            "/",
            Arc::new(OnceLock::from(None)),
            ServerMode::Http,
            None,
        )
    }

    #[tokio::test]
    async fn account_event_write_allows_progress_but_times_out_when_stalled() {
        pause();
        let event = b"event: quota-updated\ndata: {}\n\n";
        let (io, mut client) = duplex(5);
        let mut stream = account_event_stream(io).await;
        {
            let mut write = pin!(stream.write_all(event));
            let mut bytes = Vec::new();
            for expected in event.chunks(5) {
                let progress = poll_once(write.as_mut()).await;
                if bytes.len() + expected.len() == event.len() {
                    assert!(matches!(progress, Poll::Ready(Ok(()))));
                } else {
                    assert!(progress.is_pending());
                }
                let mut chunk = vec![0; expected.len()];
                client.read_exact(&mut chunk).await.unwrap();
                bytes.extend_from_slice(&chunk);
                advance(Duration::from_secs(1)).await;
            }
            assert_eq!(bytes, event);
        }
        stream.write_all(b"12345").await.unwrap();
        assert!(poll_once(pin!(stream.write(event))).await.is_pending());
        advance(Duration::from_millis(4_999)).await;
        assert!(poll_once(pin!(stream.write(event))).await.is_pending());
        advance(Duration::from_millis(2)).await;
        assert_expired_without_io(&mut stream).await;
    }

    #[tokio::test]
    async fn account_event_write_deadline_precedes_a_ready_write() {
        pause();
        let (io, mut client) = duplex(5);
        let mut stream = account_event_stream(io).await;
        stream.write_all(b"12345").await.unwrap();
        assert!(poll_once(pin!(stream.write(b"6"))).await.is_pending());
        advance(Duration::from_secs(5)).await;
        let mut chunk = [0; 5];
        client.read_exact(&mut chunk).await.unwrap();
        assert_eq!(&chunk, b"12345");
        assert_expired_without_io(&mut stream).await;
    }

    async fn assert_expired_without_io(stream: &mut Prefixed<CountingIo<impl AsyncWrite + Unpin>>) {
        let calls = stream.io.calls;
        assert!(matches!(
            poll_once(pin!(stream.write(b"6"))).await,
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        assert!(matches!(
            poll_once(pin!(stream.flush())).await,
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        assert_eq!(
            stream.io.calls, calls,
            "expired I/O must not poll the underlying writer"
        );
    }

    struct CountingIo<I> {
        inner: I,
        calls: [usize; 2],
    }

    impl<I: AsyncWrite + Unpin> AsyncWrite for CountingIo<I> {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.calls[0] += 1;
            Pin::new(&mut self.inner).poll_write(cx, bytes)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.calls[1] += 1;
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    struct FlushIo(bool);

    impl AsyncWrite for FlushIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            if self.0 {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn account_event_flush_deadline_precedes_a_ready_flush() {
        pause();
        let mut stream = account_event_stream(FlushIo(false)).await;
        assert!(poll_once(pin!(stream.flush())).await.is_pending());
        advance(Duration::from_secs(5)).await;
        stream.io.inner.0 = true;
        let calls = stream.io.calls;
        assert!(matches!(
            poll_once(pin!(stream.flush())).await,
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::TimedOut
        ));
        assert_eq!(
            stream.io.calls, calls,
            "expired flush must not poll the underlying writer"
        );
    }

    #[tokio::test]
    async fn account_event_flush_deadline_precedes_a_ready_write() {
        pause();
        let mut stream = account_event_stream(FlushIo(false)).await;
        assert!(poll_once(pin!(stream.flush())).await.is_pending());
        advance(Duration::from_secs(5)).await;
        stream.io.inner.0 = true;
        assert_expired_without_io(&mut stream).await;
    }
}
