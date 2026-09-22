use crate::MAX_PROXY_REQUEST_BYTES;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use serde_json::Value;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

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

pub struct WebSocketConnection<'a, S> {
    stream: &'a mut S,
    closed: bool,
    peer_close_code: Option<u16>,
}

impl<'a, S: Read + Write> WebSocketConnection<'a, S> {
    pub fn new(stream: &'a mut S) -> Self {
        Self {
            stream,
            closed: false,
            peer_close_code: None,
        }
    }
    pub const fn peer_close_code(&self) -> Option<u16> {
        self.peer_close_code
    }

    fn read_exact(&mut self, length: usize) -> Result<Vec<u8>, WebSocketError> {
        let mut value = vec![0u8; length];
        self.stream
            .read_exact(&mut value)
            .map_err(|_| WebSocketError::new(1006, "websocket closed"))?;
        Ok(value)
    }
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), WebSocketError> {
        if self.closed {
            return Ok(());
        }
        let mut header = Vec::with_capacity(10);
        header.push(0x80 | opcode);
        match payload.len() {
            length if length < 126 => header.push(length as u8),
            length if length <= u16::MAX as usize => {
                header.push(126);
                header.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                header.push(127);
                header.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        self.stream
            .write_all(&header)
            .and_then(|_| self.stream.write_all(payload))
            .and_then(|_| self.stream.flush())
            .map_err(|_| WebSocketError::new(1006, "websocket closed"))
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
        let mut message = Vec::new();
        let mut started = false;
        loop {
            let header = self.read_exact(2)?;
            let final_frame = header[0] & 0x80 != 0;
            if header[0] & 0x70 != 0 {
                return Err(WebSocketError::new(1002, "unsupported websocket extension"));
            }
            let opcode = header[0] & 0x0f;
            let masked = header[1] & 0x80 != 0;
            let mut length = u64::from(header[1] & 0x7f);
            if length == 126 {
                let raw = self.read_exact(2)?;
                length = u64::from(u16::from_be_bytes([raw[0], raw[1]]));
            } else if length == 127 {
                let raw = self.read_exact(8)?;
                length = u64::from_be_bytes(raw.try_into().expect("eight bytes"));
            }
            if opcode >= 8 && (!final_frame || length > 125) {
                return Err(WebSocketError::new(1002, "invalid websocket control frame"));
            }
            if !masked {
                return Err(WebSocketError::new(
                    1002,
                    "client websocket frames must be masked",
                ));
            }
            let length = usize::try_from(length)
                .map_err(|_| WebSocketError::new(1009, "websocket request is too large"))?;
            if opcode < 8
                && message
                    .len()
                    .checked_add(length)
                    .is_none_or(|size| size > MAX_PROXY_REQUEST_BYTES)
            {
                return Err(WebSocketError::new(1009, "websocket request is too large"));
            }
            let mask = self.read_exact(4)?;
            let mut payload = self.read_exact(length)?;
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
            match opcode {
                8 => {
                    if payload.len() == 1 {
                        return Err(WebSocketError::new(1002, "invalid websocket close payload"));
                    }
                    self.peer_close_code = if payload.len() >= 2 {
                        Some(u16::from_be_bytes([payload[0], payload[1]]))
                    } else {
                        Some(1005)
                    };
                    self.send_frame(8, &payload[..payload.len().min(125)])?;
                    self.closed = true;
                    return Ok(None);
                }
                9 => {
                    self.send_frame(10, &payload)?;
                    continue;
                }
                10 => continue,
                1 => {
                    if started {
                        return Err(WebSocketError::new(1002, "unexpected websocket text frame"));
                    }
                    started = true;
                }
                0 if started => {}
                _ => {
                    return Err(WebSocketError::new(
                        1003,
                        "only websocket text messages are supported",
                    ));
                }
            }
            message.extend_from_slice(&payload);
            if final_frame {
                return String::from_utf8(message)
                    .map(Some)
                    .map_err(|_| WebSocketError::new(1007, "websocket text must be valid UTF-8"));
            }
        }
    }
}

impl<S> Drop for WebSocketConnection<'_, S> {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
        }
    }
}

trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientWebSocketError {
    status: u16,
    message: &'static str,
}
impl ClientWebSocketError {
    const fn new(status: u16, message: &'static str) -> Self {
        Self { status, message }
    }
    pub const fn status(self) -> u16 {
        self.status
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
    response_headers: std::collections::BTreeMap<String, String>,
}

impl ClientWebSocket {
    pub fn connect(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        let parsed = Url::parse(url).map_err(|_| {
            ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
        })?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            return Err(ClientWebSocketError::new(
                502,
                "native upstream websocket endpoint is invalid",
            ));
        }
        let host = parsed.host_str().ok_or_else(|| {
            ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
        })?;
        let port = parsed.port_or_known_default().ok_or_else(|| {
            ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
        })?;
        let mut addresses = (host, port).to_socket_addrs().map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket connection failed")
        })?;
        let address = addresses.next().ok_or_else(|| {
            ClientWebSocketError::new(503, "native upstream websocket connection failed")
        })?;
        let tcp = TcpStream::connect_timeout(&address, timeout).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket connection failed")
        })?;
        tcp.set_read_timeout(Some(timeout)).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket connection failed")
        })?;
        tcp.set_write_timeout(Some(timeout)).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket connection failed")
        })?;
        let mut stream: Box<dyn ReadWrite> = if parsed.scheme() == "wss" {
            let loaded = rustls_native_certs::load_native_certs();
            let mut roots = rustls::RootCertStore::empty();
            for certificate in loaded.certs {
                let _ = roots.add(certificate);
            }
            if roots.is_empty() {
                return Err(ClientWebSocketError::new(
                    503,
                    "native upstream websocket TLS verification failed",
                ));
            }
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let server_name =
                rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| {
                    ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
                })?;
            let connection =
                rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|_| {
                    ClientWebSocketError::new(503, "native upstream websocket TLS setup failed")
                })?;
            Box::new(rustls::StreamOwned::new(connection, tcp))
        } else {
            Box::new(tcp)
        };
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket randomness is unavailable")
        })?;
        let key = STANDARD.encode(nonce);
        let authority = if parsed.port().is_some() {
            format!("{host}:{port}")
        } else {
            host.to_owned()
        };
        let mut path = parsed.path().to_owned();
        if path.is_empty() {
            path.push('/');
        }
        if let Some(query) = parsed.query() {
            path.push('?');
            path.push_str(query);
        }
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
        );
        for (name, value) in headers {
            let lower = name.to_ascii_lowercase();
            if matches!(
                lower.as_str(),
                "host"
                    | "upgrade"
                    | "connection"
                    | "sec-websocket-version"
                    | "sec-websocket-key"
                    | "sec-websocket-extensions"
            ) {
                continue;
            }
            if name
                .bytes()
                .any(|byte| byte == b'\r' || byte == b'\n' || byte == b':')
                || value.bytes().any(|byte| byte == b'\r' || byte == b'\n')
            {
                return Err(ClientWebSocketError::new(
                    502,
                    "native websocket header is invalid",
                ));
            }
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .and_then(|_| stream.flush())
            .map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket handshake failed")
            })?;
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= 64 * 1024 {
                return Err(ClientWebSocketError::new(
                    502,
                    "native websocket handshake is too large",
                ));
            }
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket handshake failed")
            })?;
            head.push(byte[0]);
        }
        let text = std::str::from_utf8(&head)
            .map_err(|_| ClientWebSocketError::new(502, "native websocket handshake is invalid"))?;
        let mut lines = text.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .ok_or_else(|| {
                ClientWebSocketError::new(502, "native websocket handshake is invalid")
            })?;
        if status != 101 {
            return Err(ClientWebSocketError::new(
                status,
                "native upstream websocket upgrade was rejected",
            ));
        }
        let response_headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<std::collections::BTreeMap<_, _>>();
        if response_headers
            .get("upgrade")
            .is_none_or(|value| !value.eq_ignore_ascii_case("websocket"))
            || response_headers.get("connection").is_none_or(|value| {
                !value
                    .split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case("upgrade"))
            })
            || response_headers.get("sec-websocket-accept")
                != Some(&websocket_accept(&key).map_err(|_| {
                    ClientWebSocketError::new(500, "native websocket accept failed")
                })?)
        {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket handshake is invalid",
            ));
        }
        Ok(Self {
            stream,
            closed: false,
            response_headers,
        })
    }
    pub fn response_headers(&self) -> &std::collections::BTreeMap<String, String> {
        &self.response_headers
    }
    fn read_exact(&mut self, length: usize) -> Result<Vec<u8>, ClientWebSocketError> {
        let mut value = vec![0u8; length];
        self.stream
            .read_exact(&mut value)
            .map_err(|_| ClientWebSocketError::new(502, "native upstream websocket closed"))?;
        Ok(value)
    }
    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), ClientWebSocketError> {
        let mut mask = [0u8; 4];
        getrandom::getrandom(&mut mask).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket randomness is unavailable")
        })?;
        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            length if length < 126 => frame.push(0x80 | length as u8),
            length if length <= u16::MAX as usize => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(length as u16).to_be_bytes());
            }
            length => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(length as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.stream
            .write_all(&frame)
            .and_then(|_| self.stream.flush())
            .map_err(|_| ClientWebSocketError::new(502, "native upstream websocket write failed"))
    }
    pub fn send_json(&mut self, value: &Value) -> Result<(), ClientWebSocketError> {
        let payload = serde_json::to_vec(value).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket request serialization failed")
        })?;
        if payload.len() > 4 * 1024 * 1024 {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket request is too large",
            ));
        }
        self.send_frame(1, &payload)
    }
    pub fn receive_json(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        loop {
            let header = self.read_exact(2)?;
            let final_frame = header[0] & 0x80 != 0;
            if header[0] & 0x70 != 0 {
                return Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket extension is unsupported",
                ));
            }
            let opcode = header[0] & 0x0f;
            if header[1] & 0x80 != 0 {
                return Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket frame is masked",
                ));
            }
            let mut length = u64::from(header[1] & 0x7f);
            if length == 126 {
                let raw = self.read_exact(2)?;
                length = u64::from(u16::from_be_bytes([raw[0], raw[1]]));
            } else if length == 127 {
                let raw = self.read_exact(8)?;
                length = u64::from_be_bytes(raw.try_into().expect("eight bytes"));
            }
            if !final_frame {
                return Err(ClientWebSocketError::new(
                    502,
                    "fragmented native upstream websocket event is unsupported",
                ));
            }
            let length = usize::try_from(length).map_err(|_| {
                ClientWebSocketError::new(502, "native upstream websocket event is too large")
            })?;
            if length > MAX_PROXY_REQUEST_BYTES {
                return Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket event is too large",
                ));
            }
            let payload = self.read_exact(length)?;
            match opcode {
                1 => {
                    let value: Value = serde_json::from_slice(&payload).map_err(|_| {
                        ClientWebSocketError::new(
                            502,
                            "native upstream websocket event is invalid JSON",
                        )
                    })?;
                    if !value.is_object() {
                        return Err(ClientWebSocketError::new(
                            502,
                            "native upstream websocket event is not an object",
                        ));
                    }
                    return Ok(Some(value));
                }
                8 => {
                    self.closed = true;
                    return Ok(None);
                }
                9 => {
                    self.send_frame(10, &payload)?;
                }
                10 => {}
                _ => {
                    return Err(ClientWebSocketError::new(
                        502,
                        "native upstream websocket frame type is unsupported",
                    ));
                }
            }
        }
    }
    pub fn close(&mut self) {
        if !self.closed {
            let _ = self.send_frame(8, &1000u16.to_be_bytes());
            self.closed = true;
        }
    }
}
impl Drop for ClientWebSocket {
    fn drop(&mut self) {
        self.close();
    }
}
