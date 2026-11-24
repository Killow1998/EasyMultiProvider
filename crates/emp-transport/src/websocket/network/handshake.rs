use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    time::{Sleep, sleep},
};

pub(in crate::websocket) struct HandshakeIo<S> {
    io: S,
    timeout: Option<Duration>,
    read_wait: Option<Pin<Box<Sleep>>>,
    write_wait: Option<Pin<Box<Sleep>>>,
}

impl<S> HandshakeIo<S> {
    pub(in crate::websocket) fn new(io: S, timeout: Duration) -> Self {
        Self {
            io,
            timeout: Some(timeout),
            read_wait: None,
            write_wait: None,
        }
    }

    pub(super) fn finish(&mut self) {
        self.timeout = None;
        self.read_wait = None;
        self.write_wait = None;
    }
}

impl<S: AsyncWrite + Unpin> HandshakeIo<S> {
    pub(in crate::websocket) async fn write_flushed(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes).await?;
        self.flush().await
    }
}

fn with_idle_timeout<T>(
    result: Poll<io::Result<T>>,
    timeout: Option<Duration>,
    wait: &mut Option<Pin<Box<Sleep>>>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<T>> {
    if result.is_ready() {
        *wait = None;
    } else if let Some(timeout) = timeout
        && wait
            .get_or_insert_with(|| Box::pin(sleep(timeout)))
            .as_mut()
            .poll(cx)
            .is_ready()
    {
        return Poll::Ready(Err(io::ErrorKind::TimedOut.into()));
    }
    result
}

impl<S: AsyncRead + Unpin> AsyncRead for HandshakeIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let read = Pin::new(&mut self.io).poll_read(cx, output);
        with_idle_timeout(read, self.timeout, &mut self.read_wait, cx)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for HandshakeIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let written = Pin::new(&mut self.io).poll_write(cx, bytes);
        with_idle_timeout(written, self.timeout, &mut self.write_wait, cx)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flushed = Pin::new(&mut self.io).poll_flush(cx);
        with_idle_timeout(flushed, self.timeout, &mut self.write_wait, cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
