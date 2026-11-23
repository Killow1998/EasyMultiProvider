use crate::http::{
    request::{
        BodyError, REQUEST_HEAD_DEADLINE, REQUEST_READ_TIMEOUT, parse_request, request_head_end,
    },
    response::{bad_request_response, body_error_response},
};
use std::{
    io::{self, Cursor},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf},
    time::{Instant, timeout_at},
};

#[derive(Clone)]
pub(crate) struct RawRequestHead(pub(crate) Arc<str>);

pub(super) struct GuardedHead {
    pub(super) raw: RawRequestHead,
    pub(super) release_request_permit: bool,
    bytes: Vec<u8>,
}

enum HeadError {
    Invalid,
    Framing(BodyError),
}

impl GuardedHead {
    pub(super) async fn read(
        io: &mut (impl AsyncRead + Unpin),
        accepted: Instant,
    ) -> Result<Self, Vec<u8>> {
        Self::read_before(io, accepted + REQUEST_HEAD_DEADLINE)
            .await
            .map_err(|error| match error {
                HeadError::Invalid => bad_request_response(),
                HeadError::Framing(error) => body_error_response(error),
            })
    }

    async fn read_before(
        io: &mut (impl AsyncRead + Unpin),
        deadline: Instant,
    ) -> Result<Self, HeadError> {
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(HeadError::Invalid);
            }
            let until = deadline.min(now + REQUEST_READ_TIMEOUT);
            let count = timeout_at(until, io.read(&mut buffer))
                .await
                .map_err(|_| HeadError::Invalid)?
                .map_err(|_| HeadError::Invalid)?;
            if count == 0 || Instant::now() >= deadline {
                return Err(HeadError::Invalid);
            }
            let previous_len = bytes.len();
            bytes.extend_from_slice(&buffer[..count]);
            let Some(end) =
                request_head_end(&bytes, previous_len).map_err(|()| HeadError::Invalid)?
            else {
                continue;
            };
            let head = str::from_utf8(&bytes[..end]).map_err(|_| HeadError::Invalid)?;
            let request = parse_request(head).ok_or(HeadError::Invalid)?;
            request
                .validate_body_framing()
                .map_err(HeadError::Framing)?;
            return Ok(Self {
                raw: RawRequestHead(Arc::from(head)),
                release_request_permit: request.releases_request_permit(),
                bytes,
            });
        }
    }

    pub(super) fn replay<I>(self, io: I) -> Prefixed<I> {
        Prefixed {
            io,
            prefix: Cursor::new(self.bytes),
        }
    }
}

pub(super) struct Prefixed<I> {
    io: I,
    prefix: Cursor<Vec<u8>>,
}

impl<I: AsyncRead + Unpin> AsyncRead for Prefixed<I> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let offset = self.prefix.position() as usize;
        let remaining = &self.prefix.get_ref()[offset..];
        if !remaining.is_empty() {
            let count = output.remaining().min(remaining.len());
            output.put_slice(&remaining[..count]);
            self.prefix.set_position((offset + count) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, output)
    }
}

impl<I: AsyncWrite + Unpin> AsyncWrite for Prefixed<I> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}
