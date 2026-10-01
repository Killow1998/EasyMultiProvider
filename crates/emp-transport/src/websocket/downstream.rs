//! Downstream frame I/O; no upstream dialing, proxy or compression policy.
use super::{WebSocketError, frame_length_prefix};
use crate::{
    MAX_PROXY_REQUEST_BYTES,
    websocket_pump::{FrameDecodeError, FrameDecoder, FramePoll, WebSocketPoll},
};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct WebSocketConnection<'a, S> {
    stream: &'a mut S,
    closed: bool,
    peer_close_code: Option<u16>,
    frame_decoder: FrameDecoder,
}

impl<'a, S: Read + Write> WebSocketConnection<'a, S> {
    pub fn new(stream: &'a mut S) -> Self {
        Self {
            stream,
            closed: false,
            peer_close_code: None,
            frame_decoder: FrameDecoder::new(MAX_PROXY_REQUEST_BYTES),
        }
    }

    pub fn new_with_prefix(stream: &'a mut S, read_prefix: &[u8]) -> Result<Self, WebSocketError> {
        let mut connection = Self::new(stream);
        connection.feed_read_bytes(read_prefix)?;
        Ok(connection)
    }

    pub fn feed_read_bytes(&mut self, bytes: &[u8]) -> Result<(), WebSocketError> {
        self.frame_decoder
            .feed(bytes)
            .map_err(map_downstream_frame_error)
    }
    pub const fn peer_close_code(&self) -> Option<u16> {
        self.peer_close_code
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        if self.closed {
            return Ok(());
        }
        let mut header = Vec::with_capacity(10);
        header.push(0x80 | opcode);
        header.extend(&frame_length_prefix(payload.len()));
        self.stream
            .write_all(&header)
            .and_then(|_| self.stream.write_all(payload))
            .and_then(|_| self.stream.flush())
            .map_err(|_| WebSocketError::new(1006, "websocket closed"))
    }

    pub fn send_pong(&mut self, payload: &[u8]) -> Result<(), WebSocketError> {
        if payload.len() > 125 {
            return Err(WebSocketError::new(1002, "invalid websocket ping payload"));
        }
        self.send_frame(10, payload)
    }

    pub fn set_max_message_bytes(&mut self, maximum: usize) -> Result<(), WebSocketError> {
        if maximum == 0 || maximum > MAX_PROXY_REQUEST_BYTES {
            return Err(WebSocketError::new(
                1009,
                "websocket message limit is invalid",
            ));
        }
        self.frame_decoder.set_max_message_bytes(maximum);
        Ok(())
    }

    /// Poll one downstream frame. Callers should set a short read timeout on
    /// the underlying socket and retain this connection across Pending results.
    pub fn poll_text(&mut self) -> Result<WebSocketPoll<String>, WebSocketError> {
        match self
            .frame_decoder
            .poll(self.stream, true, false)
            .map_err(map_downstream_frame_error)?
        {
            FramePoll::Pending => Ok(WebSocketPoll::Pending),
            FramePoll::Ping(payload) => Ok(WebSocketPoll::Ping(payload)),
            FramePoll::Closed { code, payload } => {
                self.peer_close_code = Some(code.unwrap_or(1005));
                let _ = self.send_frame(8, &payload);
                self.closed = true;
                Ok(WebSocketPoll::Closed { code })
            }
            FramePoll::Message {
                opcode: 1,
                payload,
                compressed: false,
            } => String::from_utf8(payload)
                .map(WebSocketPoll::Text)
                .map_err(|_| WebSocketError::new(1007, "websocket text must be valid UTF-8")),
            FramePoll::Message { .. } => Err(WebSocketError::new(
                1003,
                "only websocket text messages are supported",
            )),
        }
    }
    pub fn send_json(&mut self, value: &Value) -> Result<(), WebSocketError> {
        let encoded = serde_json::to_vec(value)
            .map_err(|_| WebSocketError::new(1011, "websocket response serialization failed"))?;
        self.send_json_bytes(&encoded)
    }
    pub fn send_json_bytes(&mut self, payload: &[u8]) -> Result<(), WebSocketError> {
        if payload.len() > MAX_PROXY_REQUEST_BYTES {
            return Err(WebSocketError::new(1009, "websocket response is too large"));
        }
        self.send_frame(1, payload)
    }
    pub fn close(&mut self, code: u16, reason: &str) {
        if self.closed {
            return;
        }
        let bytes = reason.as_bytes();
        let count = bytes.len().min(123);
        let mut payload = Vec::with_capacity(count + 2);
        payload.extend_from_slice(&code.to_be_bytes());
        payload.extend_from_slice(&bytes[..count]);
        let _ = self.send_frame(8, &payload);
        self.closed = true;
    }

    pub fn receive_text(&mut self) -> Result<Option<String>, WebSocketError> {
        loop {
            match self.poll_text()? {
                WebSocketPoll::Pending => std::thread::sleep(Duration::from_millis(1)),
                WebSocketPoll::Text(text) => return Ok(Some(text)),
                WebSocketPoll::Ping(payload) => self.send_pong(&payload)?,
                WebSocketPoll::Closed { .. } => return Ok(None),
            }
        }
    }
}

fn map_downstream_frame_error(error: FrameDecodeError) -> WebSocketError {
    match error {
        FrameDecodeError::Protocol => WebSocketError::new(1002, "invalid websocket frame"),
        FrameDecodeError::TooLarge => WebSocketError::new(1009, "websocket message is too large"),
        FrameDecodeError::Io => WebSocketError::new(1006, "websocket closed"),
    }
}

impl WebSocketConnection<'_, TcpStream> {
    pub fn set_poll_timeout(&mut self, timeout: Duration) -> Result<(), WebSocketError> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| WebSocketError::new(1011, "websocket poll timeout setup failed"))
    }
}
