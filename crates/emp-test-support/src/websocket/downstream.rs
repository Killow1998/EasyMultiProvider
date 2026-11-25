use super::WebSocketPoll;
use emp_transport::{
    MAX_PROXY_REQUEST_BYTES, MAX_RESPONSES_WEBSOCKET_BYTES, WebSocketError,
    websocket_codec::{FrameDecoder, FramePoll, frame_length_prefix, map_downstream_frame_error},
};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;

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
        connection
            .frame_decoder
            .feed(read_prefix)
            .map_err(map_downstream_frame_error)?;
        Ok(connection)
    }

    pub const fn peer_close_code(&self) -> Option<u16> {
        self.peer_close_code
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.stream
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        if self.closed {
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
        self.closed = opcode == 8 || result.is_err();
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
        self.frame_decoder.set_max_message_bytes(maximum);
        Ok(())
    }

    pub fn poll_text(&mut self) -> Result<WebSocketPoll<String>, WebSocketError> {
        if self.closed {
            return Ok(WebSocketPoll::Closed {
                code: self.peer_close_code,
            });
        }
        let poll = self
            .frame_decoder
            .poll(self.stream, true, false)
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
}

impl<S: Read + Write + polling::AsSource> WebSocketConnection<'_, S> {
    pub fn receive_text(&mut self) -> Result<Option<String>, WebSocketError> {
        loop {
            match self.poll_text()? {
                WebSocketPoll::Pending => {
                    wait_readable(self.stream)
                        .map_err(|_| WebSocketError::new(1006, "websocket closed"))?;
                }
                WebSocketPoll::Text(text) => return Ok(Some(text)),
                WebSocketPoll::Ping(payload) => self.send_pong(&payload)?,
                WebSocketPoll::Closed { .. } => return Ok(None),
            }
        }
    }
}

impl WebSocketConnection<'_, TcpStream> {
    pub fn poll_text_ready(&mut self) -> Result<WebSocketPoll<String>, WebSocketError> {
        if self.closed {
            return Ok(WebSocketPoll::Closed {
                code: self.peer_close_code,
            });
        }
        self.stream
            .set_nonblocking(true)
            .map_err(|_| WebSocketError::new(1006, "websocket closed"))?;
        let result = self.frame_decoder.poll(self.stream, true, false);
        self.stream
            .set_nonblocking(false)
            .map_err(|_| WebSocketError::new(1006, "websocket closed"))?;
        self.finish_poll(result.map_err(map_downstream_frame_error)?)
    }
}

fn wait_readable(stream: &impl polling::AsSource) -> std::io::Result<()> {
    let socket = socket2::SockRef::from(stream);
    let poller = polling::Poller::new()?;
    // SAFETY: The socket remains borrowed until it is deregistered and the poller is dropped.
    unsafe { poller.add(&*socket, polling::Event::readable(0))? };
    let mut events = polling::Events::new();
    let ready = loop {
        match poller.wait(&mut events, None) {
            Ok(0) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => break result,
        }
    };
    poller.delete(&*socket)?;
    ready.map(|_| ())
}
