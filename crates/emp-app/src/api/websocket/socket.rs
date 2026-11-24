use emp_transport::{
    MAX_RESPONSES_WEBSOCKET_BYTES, RequestBudget, RequestLimits, WebSocketError, WebSocketEvent,
    WebSocketReader, WebSocketWriter, split_websocket,
};
use serde_json::Value;
use std::{io, net::TcpStream, sync::Arc};
use tokio::{
    io::{ReadHalf, WriteHalf},
    net::TcpStream as AsyncTcpStream,
    runtime::Handle,
    sync::Mutex,
};

pub(in crate::api) fn legacy_websocket_io(
    stream: &TcpStream,
    runtime: &Handle,
) -> io::Result<AsyncTcpStream> {
    let socket = stream.try_clone()?;
    socket.set_nonblocking(true)?;
    let _runtime = runtime.enter();
    AsyncTcpStream::from_std(socket)
}

pub(super) struct Downstream {
    reader: Arc<Mutex<WebSocketReader<ReadHalf<AsyncTcpStream>>>>,
    pub(super) writer: WebSocketWriter<WriteHalf<AsyncTcpStream>>,
    pub(super) runtime: Handle,
}

pub(super) enum DownstreamEvent {
    Text(String),
    Control,
    Closed,
}

impl Downstream {
    pub(super) fn new(
        socket: AsyncTcpStream,
        prefix: &[u8],
        limits: Arc<RequestLimits>,
        runtime: Handle,
    ) -> Result<Self, WebSocketError> {
        let (reader, writer) =
            split_websocket(socket, prefix, MAX_RESPONSES_WEBSOCKET_BYTES, Some(limits))?;
        Ok(Self {
            reader: Arc::new(Mutex::new(reader)),
            writer,
            runtime,
        })
    }

    pub(super) async fn read(&self) -> Result<WebSocketEvent, WebSocketError> {
        self.reader.lock().await.read().await
    }

    pub(super) fn reader_task(&self) -> Self {
        Self {
            reader: Arc::clone(&self.reader),
            writer: self.writer.clone(),
            runtime: self.runtime.clone(),
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

    pub(super) fn take_request_budget(&self) -> Option<RequestBudget> {
        self.runtime
            .block_on(async { self.reader.lock().await.take_request_budget() })
    }

    pub(super) fn send_json(&self, event: &Value) -> Result<(), WebSocketError> {
        self.runtime.block_on(self.writer.send_json(event))
    }

    pub(super) fn close(&self, code: u16, reason: &str) {
        self.runtime.block_on(self.writer.close(code, reason));
    }
}
