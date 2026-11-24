use super::downstream::map_downstream_frame_error;
use super::frame::{FrameDecoder, FramePoll, close_payload, write_frame};
use super::{MAX_RESPONSES_WEBSOCKET_BYTES, WebSocketError, WebSocketEvent};
use crate::{RequestBudget, RequestLimits};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::Mutex;

pub struct WebSocketReader<R> {
    stream: R,
    decoder: FrameDecoder,
}

pub struct WebSocketWriter<W> {
    stream: Arc<Mutex<Writer<W>>>,
    maximum: usize,
}

struct Writer<W> {
    io: W,
    closed: bool,
}

type WebSocketParts<S> = (WebSocketReader<ReadHalf<S>>, WebSocketWriter<WriteHalf<S>>);

pub fn split_websocket<S: AsyncRead + AsyncWrite + Unpin>(
    io: S,
    prefix: &[u8],
    maximum: usize,
    limits: Option<Arc<RequestLimits>>,
) -> Result<WebSocketParts<S>, WebSocketError> {
    if maximum == 0 || maximum > MAX_RESPONSES_WEBSOCKET_BYTES {
        return Err(WebSocketError::new(
            1009,
            "websocket message limit is invalid",
        ));
    }
    let (reader, writer) = tokio::io::split(io);
    let mut decoder = FrameDecoder::new(maximum);
    decoder.feed(prefix).map_err(map_downstream_frame_error)?;
    if let Some(limits) = limits {
        decoder.limit_request_memory(limits);
    }
    Ok((
        WebSocketReader {
            stream: reader,
            decoder,
        },
        WebSocketWriter {
            stream: Arc::new(Mutex::new(Writer {
                io: writer,
                closed: false,
            })),
            maximum,
        },
    ))
}

impl<R: AsyncRead + Unpin> WebSocketReader<R> {
    pub async fn read(&mut self) -> Result<WebSocketEvent, WebSocketError> {
        match self
            .decoder
            .read(&mut self.stream, true, false)
            .await
            .map_err(map_downstream_frame_error)?
        {
            FramePoll::Pending => unreachable!("async frame reads wait for I/O"),
            FramePoll::Ping(payload) => Ok(WebSocketEvent::Ping(payload)),
            FramePoll::Closed { code, payload } => Ok(WebSocketEvent::Closed { code, payload }),
            FramePoll::Message {
                opcode: 1, payload, ..
            } => String::from_utf8(payload)
                .map(WebSocketEvent::Text)
                .map_err(|_| WebSocketError::new(1007, "websocket text must be valid UTF-8")),
            FramePoll::Message { .. } => Err(WebSocketError::new(
                1003,
                "only websocket text messages are supported",
            )),
        }
    }

    pub fn take_request_budget(&mut self) -> Option<RequestBudget> {
        self.decoder.take_request_budget()
    }
}

impl<W> Clone for WebSocketWriter<W> {
    fn clone(&self) -> Self {
        Self {
            stream: Arc::clone(&self.stream),
            maximum: self.maximum,
        }
    }
}

impl<W: AsyncWrite + Unpin> WebSocketWriter<W> {
    async fn send_frame(&self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        let mut stream = self.stream.lock().await;
        if stream.closed {
            return Err(WebSocketError::new(1006, "websocket closed"));
        }
        let result = write_frame(&mut stream.io, 0x80 | opcode, payload, None, None).await;
        stream.closed = opcode == 8 || result.is_err();
        result.map_err(|_| WebSocketError::new(1006, "websocket closed"))
    }

    pub async fn send_json(&self, value: &Value) -> Result<(), WebSocketError> {
        let encoded = serde_json::to_vec(value)
            .map_err(|_| WebSocketError::new(1011, "websocket response serialization failed"))?;
        self.send_text(&encoded).await
    }

    pub async fn send_text(&self, payload: &[u8]) -> Result<(), WebSocketError> {
        if payload.len() > self.maximum {
            return Err(WebSocketError::new(1009, "websocket response is too large"));
        }
        self.send_frame(1, payload).await
    }

    pub async fn send_pong(&self, payload: &[u8]) -> Result<(), WebSocketError> {
        if payload.len() > 125 {
            return Err(WebSocketError::new(1002, "invalid websocket ping payload"));
        }
        self.send_frame(10, payload).await
    }

    pub async fn reply_close(&self, payload: &[u8]) {
        let _ = self.send_frame(8, payload).await;
    }

    pub async fn close(&self, code: u16, reason: &str) {
        self.reply_close(&close_payload(code, reason)).await;
    }
}
