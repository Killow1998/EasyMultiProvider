use super::WebSocketPoll;
use crate::Deadline;
use emp_transport::{AsyncClientWebSocket, ClientWebSocketError, WebSocketEvent};
use serde_json::Value;
use std::{collections::BTreeMap, io, time::Duration};

pub struct ClientWebSocket {
    client: Option<AsyncClientWebSocket>,
    runtime: tokio::runtime::Runtime,
    read_timeout: Duration,
}

impl ClientWebSocket {
    pub fn connect(
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        Self::connect_with_proxy(url, headers, timeout, None)
    }

    pub fn connect_with_proxy(
        url: &str,
        headers: &BTreeMap<String, String>,
        timeout: Duration,
        proxy: Option<&str>,
    ) -> Result<Self, ClientWebSocketError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket connection failed")
            })?;
        let client = runtime.block_on(AsyncClientWebSocket::connect_with_proxy(
            url, headers, timeout, proxy,
        ))?;
        Ok(Self {
            client: Some(client),
            runtime,
            read_timeout: Duration::from_secs(300),
        })
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    pub fn set_max_message_bytes(&mut self, maximum: usize) -> Result<(), ClientWebSocketError> {
        self.client
            .as_mut()
            .ok_or_else(closed)?
            .set_max_message_bytes(maximum)
    }

    pub fn response_headers(&self) -> &BTreeMap<String, String> {
        self.client
            .as_ref()
            .expect("WebSocket is connected")
            .response_headers()
    }

    pub fn send_json(&mut self, value: &Value) -> Result<(), ClientWebSocketError> {
        let client = self.client.as_mut().ok_or_else(closed)?;
        self.runtime.block_on(client.send_json(value))
    }

    pub fn send_json_before(&mut self, value: &Value, deadline: &Deadline) -> io::Result<()> {
        let context = "WebSocket send deadline";
        deadline.accept((), context)?;
        let payload = serde_json::to_string(value).map_err(io::Error::other)?;
        let remaining = deadline.accept(deadline.remaining(), context)?;
        let client = self
            .client
            .as_mut()
            .ok_or_else(closed)
            .map_err(io::Error::other)?;
        let result = self
            .runtime
            .block_on(async { tokio::time::timeout(remaining, client.send_text(&payload)).await });
        match deadline.accept(result, context) {
            Ok(Ok(result)) => result.map_err(io::Error::other),
            Ok(Err(_)) | Err(_) => {
                self.client.take();
                Err(io::Error::new(io::ErrorKind::TimedOut, context))
            }
        }
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), ClientWebSocketError> {
        let client = self.client.as_mut().ok_or_else(closed)?;
        self.runtime.block_on(client.send_text(text))
    }

    pub fn send_pong(&mut self, payload: &[u8]) -> Result<(), ClientWebSocketError> {
        let client = self.client.as_mut().ok_or_else(closed)?;
        self.runtime.block_on(client.send_pong(payload))
    }

    pub fn poll_receive_text(&mut self) -> Result<WebSocketPoll<String>, ClientWebSocketError> {
        self.poll_receive_text_with_timeout(self.read_timeout)
    }

    pub fn poll_receive_text_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<WebSocketPoll<String>, ClientWebSocketError> {
        let Some(client) = &mut self.client else {
            return Ok(WebSocketPoll::Closed { code: None });
        };
        match self.runtime.block_on(tokio_read(client, timeout))? {
            None => Ok(WebSocketPoll::Pending),
            Some(WebSocketEvent::Text(text)) => Ok(WebSocketPoll::Text(text)),
            Some(WebSocketEvent::Ping(payload)) => Ok(WebSocketPoll::Ping(payload)),
            Some(WebSocketEvent::Closed { code, .. }) => Ok(WebSocketPoll::Closed { code }),
        }
    }

    pub fn receive_json(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        loop {
            match self.poll_receive_text()? {
                WebSocketPoll::Text(text) => {
                    if let Ok(value @ Value::Object(_)) = serde_json::from_str(&text) {
                        return Ok(Some(value));
                    }
                }
                WebSocketPoll::Ping(payload) => self.send_pong(&payload)?,
                WebSocketPoll::Closed { code } => {
                    self.close_with_code(code.unwrap_or(1000));
                    return Ok(None);
                }
                WebSocketPoll::Pending => {}
            }
        }
    }

    pub fn close(&mut self) {
        self.close_with_code(1000);
    }

    fn close_with_code(&mut self, code: u16) {
        if let Some(client) = &mut self.client {
            self.runtime.block_on(client.close(code, ""));
        }
    }
}

fn closed() -> ClientWebSocketError {
    ClientWebSocketError::new(502, "native upstream websocket write failed")
}

async fn tokio_read(
    client: &mut AsyncClientWebSocket,
    timeout: Duration,
) -> Result<Option<WebSocketEvent>, ClientWebSocketError> {
    match tokio::time::timeout(timeout, client.read()).await {
        Ok(Ok(event)) => Ok(Some(event)),
        Ok(Err(error)) => {
            client.close(1000, "").await;
            Err(error)
        }
        Err(_) => Ok(None),
    }
}

impl Drop for ClientWebSocket {
    fn drop(&mut self) {
        self.close();
    }
}
