use super::test_hooks::WriteHook;
use crate::http::request::{
    REQUEST_HEAD_DEADLINE, REQUEST_READ_TIMEOUT, RequestHead, request_head_end,
};
use socket2::SockRef;
use std::{
    io,
    net::{Shutdown, TcpStream as StdStream},
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll, ready},
};
use std::{io::Read, time::Instant};
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

pub(crate) struct HttpListener(std::net::TcpListener);

impl HttpListener {
    pub(crate) fn bind(
        host: std::net::IpAddr,
        port: u16,
    ) -> io::Result<(Self, std::net::SocketAddr)> {
        let listener = std::net::TcpListener::bind((host, port))?;
        let address = listener.local_addr()?;
        Ok((Self(listener), address))
    }

    pub(crate) fn start(
        self,
        state: Arc<crate::app::ServerState>,
    ) -> io::Result<std::thread::JoinHandle<Result<(), super::ServeError>>> {
        self.0.set_nonblocking(true)?;
        let listener = {
            let _entered = state.backend.transport.runtime.enter();
            tokio::net::TcpListener::from_std(self.0)?
        };
        std::thread::Builder::new()
            .name("emp-http".into())
            .spawn(move || {
                state
                    .backend
                    .transport
                    .runtime
                    .block_on(super::serve_http(listener, state.clone()))
            })
    }
}

/// Read once, bounded by both the per-read idle timeout and `deadline`, so a
/// peer trickling bytes cannot hold the request thread indefinitely.
fn read_before(
    stream: &mut StdStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> std::io::Result<usize> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(remaining.min(REQUEST_READ_TIMEOUT)))?;
        match stream.read(buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            result => return result,
        }
    }
}

fn restore_read_timeout(stream: &StdStream) {
    let _ = stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT));
}

pub(crate) fn read_request_head(stream: &mut StdStream) -> Option<RequestHead> {
    let head = read_request_head_before(stream, Instant::now() + REQUEST_HEAD_DEADLINE);
    restore_read_timeout(stream);
    head
}

pub(crate) fn read_request_head_before(
    stream: &mut StdStream,
    deadline: Instant,
) -> Option<RequestHead> {
    let mut buffer = [0_u8; 1024];
    let mut request = Vec::new();
    loop {
        let count = read_before(stream, &mut buffer, deadline).ok()?;
        if count == 0 {
            break;
        }
        let previous_len = request.len();
        request.extend_from_slice(&buffer[..count]);
        if let Some(end) = request_head_end(&request, previous_len).ok()? {
            let body_prefix = request.split_off(end);
            return Some(RequestHead {
                head: String::from_utf8(request).ok()?,
                body_prefix,
            });
        }
    }
    None
}
