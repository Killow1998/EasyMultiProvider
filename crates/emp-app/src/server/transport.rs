use socket2::SockRef;
use std::{
    io,
    net::{Shutdown, TcpStream as StdStream},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

#[derive(Clone)]
pub(super) struct ConnectionIo {
    stream: Arc<Mutex<TcpStream>>,
    pub(super) write_hook: WriteHook,
}

impl ConnectionIo {
    pub(super) fn new(stream: TcpStream) -> Self {
        Self {
            stream: Arc::new(Mutex::new(stream)),
            write_hook: WriteHook::default(),
        }
    }

    fn stream(&self) -> MutexGuard<'_, TcpStream> {
        self.stream.lock().expect("connection transport")
    }

    pub(super) fn set_nodelay(&self) -> io::Result<()> {
        self.stream().set_nodelay(true)
    }

    pub(super) fn probe(&self) -> io::Result<StdStream> {
        SockRef::from(&*self.stream()).try_clone().map(Into::into)
    }

    pub(super) fn cancel(&self) {
        let _ = SockRef::from(&*self.stream()).shutdown(Shutdown::Both);
        self.write_hook.cancel();
    }
}

impl AsyncRead for ConnectionIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_read(cx, buf)
    }
}

impl AsyncWrite for ConnectionIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        ready!(self.write_hook.poll(cx, buf));
        Pin::new(&mut *self.stream()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_shutdown(cx)
    }
}
use super::test_hooks::WriteHook;
