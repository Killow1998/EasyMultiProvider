use super::{MAX_RESPONSES_WEBSOCKET_BYTES, WebSocketError, frame_length_prefix};
use crate::{
    MAX_PROXY_REQUEST_BYTES, RequestBudget, RequestLimits,
    websocket_pump::{FrameDecodeError, FrameDecoder, FramePoll, WebSocketPoll},
};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct WebSocketConnection<'a, S> {
    stream: &'a mut S,
    closed: bool,
    peer_close_code: Option<u16>,
    frame_decoder: Option<FrameDecoder>,
    max_message_bytes: usize,
    frame_write_closed: Arc<Mutex<bool>>,
}

impl<'a, S: Read + Write> WebSocketConnection<'a, S> {
    pub fn new(stream: &'a mut S) -> Self {
        Self {
            stream,
            closed: false,
            peer_close_code: None,
            frame_decoder: Some(FrameDecoder::new(MAX_PROXY_REQUEST_BYTES)),
            max_message_bytes: MAX_PROXY_REQUEST_BYTES,
            frame_write_closed: Arc::new(Mutex::new(false)),
        }
    }

    pub fn new_with_prefix(stream: &'a mut S, read_prefix: &[u8]) -> Result<Self, WebSocketError> {
        let mut connection = Self::new(stream);
        connection.feed_read_bytes(read_prefix)?;
        Ok(connection)
    }

    pub fn feed_read_bytes(&mut self, bytes: &[u8]) -> Result<(), WebSocketError> {
        self.decoder()?
            .feed(bytes)
            .map_err(map_downstream_frame_error)
    }
    pub const fn peer_close_code(&self) -> Option<u16> {
        self.peer_close_code
    }

    /// Transfer the buffered and partially decoded input to one temporary reader.
    /// The original connection can write, but cannot read until it is reclaimed.
    pub fn take_reader<'b, T: Read + Write>(
        &mut self,
        stream: &'b mut T,
    ) -> Result<WebSocketConnection<'b, T>, WebSocketError> {
        let decoder = self.frame_decoder.take().ok_or(WebSocketError::new(
            1011,
            "websocket reader already transferred",
        ))?;
        Ok(WebSocketConnection {
            stream,
            closed: self.closed,
            peer_close_code: self.peer_close_code,
            frame_decoder: Some(decoder),
            max_message_bytes: self.max_message_bytes,
            frame_write_closed: Arc::clone(&self.frame_write_closed),
        })
    }

    pub fn reclaim_reader<T: Read + Write>(
        &mut self,
        mut reader: WebSocketConnection<'_, T>,
    ) -> Result<(), WebSocketError> {
        if self.frame_decoder.is_some()
            || !Arc::ptr_eq(&self.frame_write_closed, &reader.frame_write_closed)
        {
            return Err(WebSocketError::new(1011, "unrelated websocket reader"));
        }
        self.frame_decoder = reader.frame_decoder.take();
        self.closed |= reader.closed;
        self.peer_close_code = reader.peer_close_code;
        Ok(())
    }

    fn decoder(&mut self) -> Result<&mut FrameDecoder, WebSocketError> {
        self.frame_decoder.as_mut().ok_or(WebSocketError::new(
            1011,
            "websocket read owner is elsewhere",
        ))
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        let mut frame_write_closed = self
            .frame_write_closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.closed || *frame_write_closed {
            return Err(WebSocketError::new(1006, "websocket closed"));
        }
        let mut header = Vec::with_capacity(10);
        header.push(0x80 | opcode);
        header.extend(&frame_length_prefix(payload.len()));
        let result = self
            .stream
            .write_all(&header)
            .and_then(|_| self.stream.write_all(payload))
            .and_then(|_| self.stream.flush())
            .map_err(|_| WebSocketError::new(1006, "websocket closed"));
        *frame_write_closed |= opcode == 8 || result.is_err();
        result
    }

    pub fn send_pong(&mut self, payload: &[u8]) -> Result<(), WebSocketError> {
        if payload.len() > 125 {
            return Err(WebSocketError::new(1002, "invalid websocket ping payload"));
        }
        self.send_frame(10, payload)
    }

    pub fn set_max_message_bytes(&mut self, maximum: usize) -> Result<(), WebSocketError> {
        if maximum == 0 || maximum > MAX_RESPONSES_WEBSOCKET_BYTES {
            return Err(WebSocketError::new(
                1009,
                "websocket message limit is invalid",
            ));
        }
        self.decoder()?.set_max_message_bytes(maximum);
        Ok(())
    }

    pub fn enable_responses_capacity(&mut self, limits: Arc<RequestLimits>) {
        let decoder = self
            .frame_decoder
            .as_mut()
            .expect("new websocket connection");
        decoder.set_max_message_bytes(MAX_RESPONSES_WEBSOCKET_BYTES);
        decoder.limit_request_memory(limits);
        self.max_message_bytes = MAX_RESPONSES_WEBSOCKET_BYTES;
    }

    pub fn take_request_budget(&mut self) -> Option<RequestBudget> {
        self.frame_decoder
            .as_mut()
            .and_then(FrameDecoder::take_request_budget)
    }

    pub fn poll_text(&mut self) -> Result<WebSocketPoll<String>, WebSocketError> {
        self.poll_with(|decoder, stream, _| decoder.poll(stream, true, false))
    }

    fn poll_with(
        &mut self,
        read: impl FnOnce(
            &mut FrameDecoder,
            &mut S,
            &Mutex<bool>,
        ) -> Result<FramePoll, FrameDecodeError>,
    ) -> Result<WebSocketPoll<String>, WebSocketError> {
        if self.closed {
            return Ok(WebSocketPoll::Closed {
                code: self.peer_close_code,
            });
        }
        let decoder = self.frame_decoder.as_mut().ok_or(WebSocketError::new(
            1011,
            "websocket read owner is elsewhere",
        ))?;
        let poll = read(decoder, self.stream, &self.frame_write_closed)
            .map_err(map_downstream_frame_error)?;
        self.finish_poll(poll)
    }

    fn finish_poll(&mut self, poll: FramePoll) -> Result<WebSocketPoll<String>, WebSocketError> {
        match poll {
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
        if payload.len() > self.max_message_bytes {
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
        FrameDecodeError::AtCapacity => {
            WebSocketError::new(1013, "not enough memory for websocket message")
        }
        FrameDecodeError::Io => WebSocketError::new(1006, "websocket closed"),
    }
}

impl WebSocketConnection<'_, TcpStream> {
    pub fn poll_text_ready(&mut self) -> Result<WebSocketPoll<String>, WebSocketError> {
        self.poll_with(|decoder, stream, frame_write_closed| {
            decoder.poll(
                &mut ReadyReader {
                    stream,
                    frame_write_closed,
                },
                true,
                false,
            )
        })
    }

    pub fn set_poll_timeout(&mut self, timeout: Duration) -> Result<(), WebSocketError> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|_| WebSocketError::new(1011, "websocket poll timeout setup failed"))
    }
}

struct ReadyReader<'a> {
    stream: &'a mut TcpStream,
    frame_write_closed: &'a Mutex<bool>,
}

impl Read for ReadyReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let _writer = self
            .frame_write_closed
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.stream.set_nonblocking(true)?;
        let result = self.stream.read(buffer);
        self.stream.set_nonblocking(false)?;
        result
    }
}
