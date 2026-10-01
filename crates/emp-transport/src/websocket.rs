//! WebSocket public types and shared RFC 6455 handshake/framing primitives.
use crate::websocket_pump::FrameDecoder;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use compression::PerMessageDeflate;
use network::ReadWrite;
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use std::fmt;

mod client;
mod compression;
mod downstream;
mod network;
#[cfg(test)]
mod tests;

pub use downstream::WebSocketConnection;

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebSocketError {
    code: u16,
    message: &'static str,
}

impl WebSocketError {
    const fn new(code: u16, message: &'static str) -> Self {
        Self { code, message }
    }
    pub const fn close_code(self) -> u16 {
        self.code
    }
}

/// RFC 6455 payload length prefix: 7-bit inline, 126 + u16, or 127 + u64.
fn frame_length_prefix(length: usize) -> Vec<u8> {
    if length < 126 {
        vec![length as u8]
    } else if length <= u16::MAX as usize {
        let mut bytes = vec![126];
        bytes.extend_from_slice(&(length as u16).to_be_bytes());
        bytes
    } else {
        let mut bytes = vec![127];
        bytes.extend_from_slice(&(length as u64).to_be_bytes());
        bytes
    }
}

impl fmt::Display for WebSocketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}
impl std::error::Error for WebSocketError {}

pub fn websocket_accept(key: &str) -> Result<String, WebSocketError> {
    let raw = STANDARD
        .decode(key.as_bytes())
        .map_err(|_| WebSocketError::new(1002, "invalid Sec-WebSocket-Key"))?;
    if raw.len() != 16 {
        return Err(WebSocketError::new(1002, "invalid Sec-WebSocket-Key"));
    }
    let mut source = Vec::with_capacity(key.len() + WEBSOCKET_GUID.len());
    source.extend_from_slice(key.as_bytes());
    source.extend_from_slice(WEBSOCKET_GUID.as_bytes());
    Ok(STANDARD.encode(digest(&SHA1_FOR_LEGACY_USE_ONLY, &source).as_ref()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientWebSocketError {
    status: u16,
    message: &'static str,
    upgrade_rejection: bool,
}
impl ClientWebSocketError {
    pub(crate) const fn new(status: u16, message: &'static str) -> Self {
        Self {
            status,
            message,
            upgrade_rejection: false,
        }
    }
    const fn upgrade_rejected(status: u16, message: &'static str) -> Self {
        Self {
            status,
            message,
            upgrade_rejection: true,
        }
    }
    pub const fn status(self) -> u16 {
        self.status
    }
    pub const fn is_upgrade_rejection(self) -> bool {
        self.upgrade_rejection
    }
}
impl fmt::Display for ClientWebSocketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}
impl std::error::Error for ClientWebSocketError {}

pub struct ClientWebSocket {
    stream: Box<dyn ReadWrite>,
    closed: bool,
    peer_close_code: Option<u16>,
    response_headers: std::collections::BTreeMap<String, String>,
    compression: Option<PerMessageDeflate>,
    max_message_bytes: usize,
    frame_decoder: FrameDecoder,
    local_control: bool,
}

fn local_socket_error(error: std::io::Error) -> ClientWebSocketError {
    use std::io::ErrorKind;
    let status = match error.kind() {
        ErrorKind::PermissionDenied => 403,
        ErrorKind::TimedOut | ErrorKind::WouldBlock => 504,
        ErrorKind::NotFound
        | ErrorKind::ConnectionRefused
        | ErrorKind::ConnectionReset
        | ErrorKind::UnexpectedEof => 503,
        _ => 500,
    };
    ClientWebSocketError::new(status, "local Codex control socket is unavailable")
}
