use socket2::SockRef;
use std::{
    io,
    net::{Shutdown, TcpStream as StdStream},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

#[derive(Clone)]
pub(super) struct ConnectionIo {
    stream: Arc<Mutex<TcpStream>>,
    #[cfg(test)]
    write_probe: Arc<Mutex<Option<super::tests::WriteProbe>>>,
}

impl ConnectionIo {
    pub(super) fn new(stream: TcpStream) -> Self {
        Self {
            stream: Arc::new(Mutex::new(stream)),
            #[cfg(test)]
            write_probe: Arc::default(),
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
        #[cfg(test)]
        self.write_probe.lock().unwrap().take();
    }

    #[cfg(test)]
    pub(super) fn probe_write(&self, probe: super::tests::WriteProbe) {
        *self.write_probe.lock().unwrap() = Some(probe);
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
        #[cfg(test)]
        {
            let mut armed = self.write_probe.lock().unwrap();
            if let Some(probe) = armed.as_mut()
                && (probe.reached.is_none()
                    || buf
                        .windows(probe.marker.len())
                        .any(|bytes| bytes == probe.marker))
            {
                if let Some(reached) = probe.reached.take() {
                    let _ = reached.send(());
                }
                std::task::ready!(Pin::new(&mut probe.release).poll(cx)).ok();
                armed.take();
            }
        }
        Pin::new(&mut *self.stream()).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream()).poll_shutdown(cx)
    }
}
