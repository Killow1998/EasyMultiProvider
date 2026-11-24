use emp_transport::{
    MAX_RESPONSES_WEBSOCKET_BYTES, RequestBudget, RequestLimits, WebSocketError, WebSocketEvent,
    WebSocketReader, WebSocketWriter, split_websocket,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf},
    sync::Mutex,
};

pub(super) trait WebSocketIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> WebSocketIo for T {}

type Socket = Box<dyn WebSocketIo>;
type Reader = WebSocketReader<ReadHalf<Socket>>;

pub(super) struct Downstream {
    reader: Arc<Mutex<Reader>>,
    pub(super) writer: WebSocketWriter<WriteHalf<Socket>>,
}

pub(super) enum DownstreamEvent {
    Text(String),
    Control,
    Closed,
}

impl Downstream {
    pub(super) fn new(
        socket: impl AsyncRead + AsyncWrite + Unpin + Send + 'static,
        prefix: &[u8],
        limits: Arc<RequestLimits>,
    ) -> Result<Self, WebSocketError> {
        let (reader, writer) = split_websocket(
            Box::new(socket) as Socket,
            prefix,
            MAX_RESPONSES_WEBSOCKET_BYTES,
            Some(limits),
        )?;
        Ok(Self {
            reader: Arc::new(Mutex::new(reader)),
            writer,
        })
    }

    pub(super) async fn read(&self) -> Result<WebSocketEvent, WebSocketError> {
        self.reader.lock().await.read().await
    }

    pub(super) fn reader_task(&self) -> Self {
        Self {
            reader: Arc::clone(&self.reader),
            writer: self.writer.clone(),
        }
    }

    pub(super) async fn finish_read(
        &self,
        event: Result<WebSocketEvent, WebSocketError>,
    ) -> DownstreamEvent {
        match event {
            Ok(WebSocketEvent::Text(text)) => DownstreamEvent::Text(text),
            Ok(WebSocketEvent::Ping(payload)) => {
                if self.writer.send_pong(&payload).await.is_ok() {
                    DownstreamEvent::Control
                } else {
                    DownstreamEvent::Closed
                }
            }
            Ok(WebSocketEvent::Closed { payload, .. }) => {
                self.writer.reply_close(&payload).await;
                DownstreamEvent::Closed
            }
            Err(error) => {
                self.writer
                    .close(error.close_code(), &error.to_string())
                    .await;
                DownstreamEvent::Closed
            }
        }
    }

    pub(super) async fn take_request_budget(&self) -> Option<RequestBudget> {
        self.reader.lock().await.take_request_budget()
    }

    pub(super) async fn send_json(&self, event: &Value) -> Result<(), WebSocketError> {
        self.writer.send_json(event).await
    }

    pub(super) async fn close(&self, code: u16, reason: &str) {
        self.writer.close(code, reason).await;
    }
}
