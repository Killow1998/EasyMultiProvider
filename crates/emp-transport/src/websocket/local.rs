use super::client::{client_mask, client_write_error, upgrade_request, upgrade_response};
use super::frame::{FrameDecoder, FramePoll, close_payload};
use super::{ClientWebSocketError, frame_length_prefix, local_socket_error};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    time::Duration,
};

pub struct LocalWebSocket {
    socket: socket2::Socket,
    decoder: FrameDecoder,
    closed: bool,
}

impl LocalWebSocket {
    /// Read-only Codex control transport. It never consults proxy settings.
    pub fn connect(
        path: &std::path::Path,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        std::fs::metadata(path).map_err(local_socket_error)?;
        let address = socket2::SockAddr::unix(path).map_err(local_socket_error)?;
        let mut socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
            .map_err(local_socket_error)?;
        socket
            .connect_timeout(&address, timeout)
            .map_err(local_socket_error)?;
        socket
            .set_read_timeout(Some(timeout))
            .map_err(local_socket_error)?;
        socket
            .set_write_timeout(Some(timeout))
            .map_err(local_socket_error)?;
        let url = url::Url::parse("ws://localhost/").expect("constant local WebSocket URL");
        let (request, key) = upgrade_request(&url, &BTreeMap::new(), false, None, false)?;
        let handshake_error =
            |_| ClientWebSocketError::new(503, "native upstream websocket handshake failed");
        socket
            .write_all(request.as_bytes())
            .and_then(|()| socket.flush())
            .map_err(handshake_error)?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= 64 * 1024 {
                return Err(ClientWebSocketError::new(
                    502,
                    "native websocket handshake is too large",
                ));
            }
            let mut byte = [0];
            socket.read_exact(&mut byte).map_err(handshake_error)?;
            head.push(byte[0]);
        }
        if upgrade_response(&head, &key)?.contains_key("sec-websocket-extensions") {
            return Err(ClientWebSocketError::new(
                502,
                "local control websocket returned an unsolicited extension",
            ));
        }
        Ok(Self {
            socket,
            decoder: FrameDecoder::new(2 * 1024 * 1024),
            closed: false,
        })
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), ClientWebSocketError> {
        let mask = client_mask()?;
        let mut frame = vec![0x80 | opcode];
        frame.extend(frame_length_prefix(payload.len()));
        frame[1] |= 0x80;
        frame.extend(mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.socket
            .write_all(&frame)
            .and_then(|()| self.socket.flush())
            .map_err(client_write_error)
    }

    pub fn send_json(&mut self, value: &Value) -> Result<(), ClientWebSocketError> {
        let payload = serde_json::to_vec(value).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket request serialization failed")
        })?;
        if payload.len() > 2 * 1024 * 1024 {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket request is too large",
            ));
        }
        self.send_frame(1, &payload)
    }

    /// Control RPC skips unrelated JSON values; native Responses requires objects.
    pub fn receive_value(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        use super::frame::FrameDecodeError;
        loop {
            match self.decoder.poll(&mut self.socket, false, false) {
                Ok(FramePoll::Pending) => {}
                Ok(FramePoll::Ping(payload)) => self.send_frame(10, &payload)?,
                Ok(FramePoll::Closed { code, .. }) => {
                    self.close(code.unwrap_or(1000), "");
                    return Ok(None);
                }
                Ok(FramePoll::Message {
                    opcode: 1, payload, ..
                }) => {
                    let text = String::from_utf8(payload).map_err(|_| {
                        self.close(1007, "text must be UTF-8");
                        ClientWebSocketError::new(
                            502,
                            "native upstream websocket event is not UTF-8",
                        )
                    })?;
                    return serde_json::from_str(&text).map(Some).map_err(|_| {
                        ClientWebSocketError::new(
                            502,
                            "local control websocket event is invalid JSON",
                        )
                    });
                }
                Ok(FramePoll::Message { .. }) => {
                    self.close(1003, "unsupported data frame");
                    return Err(ClientWebSocketError::new(
                        502,
                        "native upstream websocket frame type is unsupported",
                    ));
                }
                Err(FrameDecodeError::Protocol) => {
                    self.close(1002, "invalid frame");
                    return Err(ClientWebSocketError::new(
                        502,
                        "native upstream websocket frame is invalid",
                    ));
                }
                Err(FrameDecodeError::TooLarge) => {
                    self.close(1009, "message too large");
                    return Err(ClientWebSocketError::new(
                        502,
                        "native upstream websocket event is too large",
                    ));
                }
                Err(FrameDecodeError::AtCapacity) => {
                    return Err(ClientWebSocketError::new(
                        503,
                        "not enough memory for websocket message",
                    ));
                }
                Err(FrameDecodeError::Io) => {
                    return Err(ClientWebSocketError::new(
                        503,
                        "native upstream websocket closed",
                    ));
                }
            }
        }
    }

    fn close(&mut self, code: u16, reason: &str) {
        if !self.closed {
            let _ = self.send_frame(8, &close_payload(code, reason));
            self.closed = true;
        }
    }
}

impl Drop for LocalWebSocket {
    fn drop(&mut self) {
        self.close(1000, "");
    }
}
