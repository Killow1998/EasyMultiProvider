use crate::MAX_PROXY_REQUEST_BYTES;
use crate::websocket_pump::{FrameDecodeError, FrameDecoder, FramePoll, WebSocketPoll};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use serde_json::Value;
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

mod client;

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

impl<S> Drop for WebSocketConnection<'_, S> {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
        }
    }
}

trait ReadWrite: Read + Write + Send {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
}
type OpenedWebSocketTransport = (Box<dyn ReadWrite>, bool, Option<String>);
impl ReadWrite for socket2::Socket {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        socket2::Socket::set_read_timeout(self, timeout)
    }
}
impl ReadWrite for TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
}
impl<S: ReadWrite> ReadWrite for rustls::StreamOwned<rustls::ClientConnection, S> {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.sock.set_read_timeout(timeout)
    }
}
impl ReadWrite for Box<dyn ReadWrite> {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.as_ref().set_read_timeout(timeout)
    }
}

fn connect_tcp(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, ClientWebSocketError> {
    let addresses = (host, port).to_socket_addrs().map_err(|_| {
        ClientWebSocketError::new(503, "native upstream websocket connection failed")
    })?;
    for address in addresses {
        if let Ok(tcp) = TcpStream::connect_timeout(&address, timeout) {
            tcp.set_read_timeout(Some(timeout)).map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket connection failed")
            })?;
            tcp.set_write_timeout(Some(timeout)).map_err(|_| {
                ClientWebSocketError::new(503, "native upstream websocket connection failed")
            })?;
            return Ok(tcp);
        }
    }
    Err(ClientWebSocketError::new(
        503,
        "native upstream websocket connection failed",
    ))
}

fn tls_stream(
    stream: Box<dyn ReadWrite>,
    host: &str,
) -> Result<Box<dyn ReadWrite>, ClientWebSocketError> {
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
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned()).map_err(|_| {
        ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
    })?;
    let connection =
        rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket TLS setup failed")
        })?;
    Ok(Box::new(rustls::StreamOwned::new(connection, stream)))
}

fn read_http_head(stream: &mut dyn ReadWrite) -> Result<Vec<u8>, ClientWebSocketError> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= 64 * 1024 {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket handshake is too large",
            ));
        }
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket handshake failed")
        })?;
        head.push(byte[0]);
    }
    Ok(head)
}

fn proxy_authorization(proxy: &Url) -> Option<String> {
    let username = proxy.username();
    let password = proxy.password();
    (!username.is_empty() || password.is_some()).then(|| {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{username}:{}", password.unwrap_or_default()))
        )
    })
}

fn socks_connect(
    stream: &mut dyn ReadWrite,
    proxy: &Url,
    host: &str,
    port: u16,
) -> Result<(), ClientWebSocketError> {
    let credential = (!proxy.username().is_empty() || proxy.password().is_some())
        .then(|| (proxy.username(), proxy.password().unwrap_or_default()));
    let methods: &[u8] = if credential.is_some() { &[0, 2] } else { &[0] };
    stream
        .write_all(&[&[5, methods.len() as u8], methods].concat())
        .and_then(|_| stream.flush())
        .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
    let mut selected = [0_u8; 2];
    stream
        .read_exact(&mut selected)
        .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
    if selected[0] != 5 || selected[1] == 255 {
        return Err(ClientWebSocketError::new(
            503,
            "native websocket proxy rejected authentication",
        ));
    }
    if selected[1] == 2 {
        let (username, password) = credential.ok_or_else(|| {
            ClientWebSocketError::new(503, "native websocket proxy rejected authentication")
        })?;
        if username.len() > 255 || password.len() > 255 {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket proxy credential is invalid",
            ));
        }
        let mut request = vec![1, username.len() as u8];
        request.extend_from_slice(username.as_bytes());
        request.push(password.len() as u8);
        request.extend_from_slice(password.as_bytes());
        stream
            .write_all(&request)
            .and_then(|_| stream.flush())
            .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
        let mut response = [0_u8; 2];
        stream
            .read_exact(&mut response)
            .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
        if response != [1, 0] {
            return Err(ClientWebSocketError::new(
                503,
                "native websocket proxy rejected authentication",
            ));
        }
    } else if selected[1] != 0 {
        return Err(ClientWebSocketError::new(
            503,
            "native websocket proxy selected an unsupported method",
        ));
    }
    if host.len() > 255 {
        return Err(ClientWebSocketError::new(
            502,
            "native upstream websocket endpoint is invalid",
        ));
    }
    let mut request = vec![5, 1, 0, 3, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream
        .write_all(&request)
        .and_then(|_| stream.flush())
        .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
    let mut response = [0_u8; 4];
    stream
        .read_exact(&mut response)
        .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
    if response[0] != 5 || response[1] != 0 {
        return Err(ClientWebSocketError::new(
            503,
            "native websocket proxy rejected the connection",
        ));
    }
    let address_bytes = match response[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0_u8; 1];
            stream
                .read_exact(&mut length)
                .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
            usize::from(length[0])
        }
        _ => {
            return Err(ClientWebSocketError::new(
                503,
                "native websocket proxy returned an invalid response",
            ));
        }
    };
    let mut ignored = vec![0_u8; address_bytes + 2];
    stream
        .read_exact(&mut ignored)
        .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))
}

fn websocket_connection(
    target: &Url,
    proxy: Option<&str>,
    timeout: Duration,
) -> Result<OpenedWebSocketTransport, ClientWebSocketError> {
    let host = target.host_str().ok_or_else(|| {
        ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
    })?;
    let port = target.port_or_known_default().ok_or_else(|| {
        ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
    })?;
    let Some(proxy) = proxy else {
        let tcp = connect_tcp(host, port, timeout)?;
        let stream: Box<dyn ReadWrite> = Box::new(tcp);
        return Ok((
            if target.scheme() == "wss" {
                tls_stream(stream, host)?
            } else {
                stream
            },
            false,
            None,
        ));
    };
    let proxy = Url::parse(proxy)
        .map_err(|_| ClientWebSocketError::new(502, "native websocket proxy is invalid"))?;
    let proxy_host = proxy
        .host_str()
        .ok_or_else(|| ClientWebSocketError::new(502, "native websocket proxy is invalid"))?;
    let proxy_port = proxy
        .port_or_known_default()
        .ok_or_else(|| ClientWebSocketError::new(502, "native websocket proxy is invalid"))?;
    let tcp = connect_tcp(proxy_host, proxy_port, timeout)?;
    let mut stream: Box<dyn ReadWrite> = Box::new(tcp);
    if proxy.scheme() == "https" {
        stream = tls_stream(stream, proxy_host)?;
    }
    if matches!(proxy.scheme(), "socks5" | "socks5h") {
        socks_connect(stream.as_mut(), &proxy, host, port)?;
        if target.scheme() == "wss" {
            stream = tls_stream(stream, host)?;
        }
        return Ok((stream, false, None));
    }
    if !matches!(proxy.scheme(), "http" | "https") {
        return Err(ClientWebSocketError::new(
            502,
            "native websocket proxy is unsupported",
        ));
    }
    let authorization = proxy_authorization(&proxy);
    if target.scheme() == "wss" {
        let authority = format!("{host}:{port}");
        let auth = authorization
            .as_deref()
            .map(|value| format!("Proxy-Authorization: {value}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{auth}Connection: keep-alive\r\n\r\n"
        )
        .and_then(|_| stream.flush())
        .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
        let head = read_http_head(stream.as_mut())?;
        let status = std::str::from_utf8(&head)
            .ok()
            .and_then(|value| value.lines().next())
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|status| status.parse::<u16>().ok())
            .unwrap_or(502);
        if status != 200 {
            return Err(ClientWebSocketError::new(
                status,
                "native websocket proxy rejected the connection",
            ));
        }
        stream = tls_stream(stream, host)?;
        Ok((stream, false, None))
    } else {
        Ok((stream, true, authorization))
    }
}

struct PerMessageDeflate {
    compressor: Compress,
    decompressor: Decompress,
    client_no_context_takeover: bool,
    server_no_context_takeover: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecompressionError {
    Invalid,
    TooLarge,
}

impl PerMessageDeflate {
    fn negotiated(value: &str) -> Result<Self, ClientWebSocketError> {
        let mut parts = value.split(';').map(str::trim);
        if parts.next() != Some("permessage-deflate") {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket extension is unsupported",
            ));
        }
        let mut client_bits = 15_u8;
        let mut server_bits = 15_u8;
        let mut client_no_context_takeover = false;
        let mut server_no_context_takeover = false;
        for parameter in parts {
            if parameter == "client_no_context_takeover" {
                client_no_context_takeover = true;
            } else if parameter == "server_no_context_takeover" {
                server_no_context_takeover = true;
            } else if let Some(value) = parameter.strip_prefix("client_max_window_bits=") {
                client_bits = value.parse().map_err(|_| {
                    ClientWebSocketError::new(502, "native websocket extension is invalid")
                })?;
            } else if let Some(value) = parameter.strip_prefix("server_max_window_bits=") {
                server_bits = value.parse().map_err(|_| {
                    ClientWebSocketError::new(502, "native websocket extension is invalid")
                })?;
            } else if !parameter.is_empty() {
                return Err(ClientWebSocketError::new(
                    502,
                    "native websocket extension is unsupported",
                ));
            }
        }
        if !(9..=15).contains(&client_bits) || !(9..=15).contains(&server_bits) {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket extension is invalid",
            ));
        }
        Ok(Self {
            compressor: Compress::new_with_window_bits(Compression::fast(), false, client_bits),
            decompressor: Decompress::new_with_window_bits(false, server_bits),
            client_no_context_takeover,
            server_no_context_takeover,
        })
    }

    fn compress(&mut self, payload: &[u8]) -> Result<Vec<u8>, ClientWebSocketError> {
        let before_in = self.compressor.total_in();
        let mut output = Vec::with_capacity(payload.len().saturating_add(64));
        loop {
            output.reserve(8192);
            let consumed = usize::try_from(self.compressor.total_in() - before_in)
                .unwrap_or(payload.len())
                .min(payload.len());
            self.compressor
                .compress_vec(&payload[consumed..], &mut output, FlushCompress::Sync)
                .map_err(|_| {
                    ClientWebSocketError::new(502, "native websocket compression failed")
                })?;
            let consumed =
                usize::try_from(self.compressor.total_in() - before_in).unwrap_or(payload.len());
            if consumed >= payload.len() && output.ends_with(&[0, 0, 255, 255]) {
                output.truncate(output.len() - 4);
                break;
            }
            if output.len() > MAX_PROXY_REQUEST_BYTES {
                return Err(ClientWebSocketError::new(
                    413,
                    "native websocket request is too large",
                ));
            }
        }
        if self.client_no_context_takeover {
            self.compressor.reset();
        }
        Ok(output)
    }

    fn decompress(
        &mut self,
        payload: &[u8],
        max_message_bytes: usize,
    ) -> Result<Vec<u8>, DecompressionError> {
        let mut encoded = Vec::with_capacity(payload.len() + 4);
        encoded.extend_from_slice(payload);
        encoded.extend_from_slice(&[0, 0, 255, 255]);
        let before_in = self.decompressor.total_in();
        let mut output = Vec::with_capacity(max_message_bytes.saturating_add(1).min(8192));
        loop {
            let remaining = max_message_bytes
                .saturating_add(1)
                .saturating_sub(output.len());
            let wanted = remaining.min(8192);
            let spare = output.capacity().saturating_sub(output.len());
            if spare < wanted {
                output
                    .try_reserve_exact(wanted - spare)
                    .map_err(|_| DecompressionError::TooLarge)?;
            }
            let consumed = usize::try_from(self.decompressor.total_in() - before_in)
                .unwrap_or(encoded.len())
                .min(encoded.len());
            self.decompressor
                .decompress_vec(&encoded[consumed..], &mut output, FlushDecompress::Sync)
                .map_err(|_| DecompressionError::Invalid)?;
            if output.len() > max_message_bytes {
                return Err(DecompressionError::TooLarge);
            }
            let consumed =
                usize::try_from(self.decompressor.total_in() - before_in).unwrap_or(encoded.len());
            if consumed >= encoded.len() {
                break;
            }
        }
        if self.server_no_context_takeover {
            self.decompressor.reset(false);
        }
        Ok(output)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket_pump::WebSocketPumpConfig;
    use flate2::{Compress, Compression, FlushCompress};
    use std::io::Cursor;

    impl ReadWrite for Cursor<Vec<u8>> {
        fn set_read_timeout(&self, _timeout: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn client_socket() -> ClientWebSocket {
        ClientWebSocket {
            stream: Box::new(Cursor::new(Vec::new())),
            closed: false,
            peer_close_code: None,
            response_headers: Default::default(),
            compression: None,
            max_message_bytes: MAX_PROXY_REQUEST_BYTES,
            frame_decoder: FrameDecoder::new(MAX_PROXY_REQUEST_BYTES),
            local_control: false,
        }
    }

    #[test]
    fn client_websocket_error_marks_only_real_upgrade_rejections() {
        assert!(!ClientWebSocketError::new(503, "transport failed").is_upgrade_rejection());
        let rejected = ClientWebSocketError::upgrade_rejected(404, "upgrade rejected");
        assert_eq!(rejected.status(), 404);
        assert!(rejected.is_upgrade_rejection());
    }

    fn masked_data_header(length: u64) -> Vec<u8> {
        let mut bytes = vec![0x81, 0xff];
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        bytes
    }

    fn masked_text_frame(text: &str) -> Vec<u8> {
        let mask = [0x11, 0x22, 0x33, 0x44];
        let bytes = text.as_bytes();
        let mut frame = vec![0x81, 0x80 | bytes.len() as u8];
        frame.extend_from_slice(&mask);
        frame.extend(
            bytes
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % mask.len()]),
        );
        frame
    }

    #[test]
    fn regular_websockets_keep_legacy_cap_while_sideband_can_use_four_mib() {
        const FOUR_MIB: usize = 4 * 1024 * 1024;
        let declared_length = (FOUR_MIB + 1) as u64;

        let mut legacy_bytes = Cursor::new(masked_data_header(declared_length));
        let mut legacy = WebSocketConnection::new(&mut legacy_bytes);
        assert!(
            matches!(
                legacy.poll_text().unwrap(),
                WebSocketPoll::Closed { code: None }
            ),
            "the legacy cap accepts a 4 MiB+1 declaration"
        );

        let mut sideband_bytes = Cursor::new(masked_data_header(declared_length));
        let mut sideband = WebSocketConnection::new(&mut sideband_bytes);
        sideband.set_max_message_bytes(FOUR_MIB).unwrap();
        assert_eq!(sideband.poll_text().unwrap_err().close_code(), 1009);

        let client = client_socket();
        assert_eq!(client.max_message_bytes, MAX_PROXY_REQUEST_BYTES);
        assert_eq!(WebSocketPumpConfig::default().max_message_bytes, FOUR_MIB);
    }

    #[test]
    fn downstream_upgrade_prefix_preserves_first_frame() {
        let prefix = masked_text_frame("first-frame");
        let mut stream = Cursor::new(Vec::new());
        let mut connection = WebSocketConnection::new_with_prefix(&mut stream, &prefix).unwrap();
        assert!(matches!(
            connection.poll_text().unwrap(),
            WebSocketPoll::Text(text) if text == "first-frame"
        ));
    }

    #[test]
    fn downstream_ping_is_returned_for_single_owner_pong_and_empty_close_is_preserved() {
        let mut ping = vec![0x89, 0x80 | 4, 1, 2, 3, 4];
        ping.extend([b'p' ^ 1, b'i' ^ 2, b'n' ^ 3, b'g' ^ 4]);
        let mut stream = Cursor::new(Vec::new());
        let mut connection = WebSocketConnection::new_with_prefix(&mut stream, &ping).unwrap();
        assert!(matches!(
            connection.poll_text().unwrap(),
            WebSocketPoll::Ping(payload) if payload == b"ping"
        ));
        connection.send_pong(b"ping").unwrap();
        drop(connection);
        assert_eq!(stream.into_inner(), [0x8a, 4, b'p', b'i', b'n', b'g']);

        let empty_close = vec![0x88, 0x80, 5, 6, 7, 8];
        let mut stream = Cursor::new(Vec::new());
        let mut connection =
            WebSocketConnection::new_with_prefix(&mut stream, &empty_close).unwrap();
        assert_eq!(
            connection.poll_text().unwrap(),
            WebSocketPoll::Closed { code: None }
        );
        assert_eq!(connection.peer_close_code(), Some(1005));
        drop(connection);
        assert_eq!(stream.into_inner(), [0x88, 0]);
    }

    #[test]
    fn deflate_output_is_incrementally_limited_and_errors_are_classified() {
        let input = vec![b'x'; 4 * 1024 * 1024 + 1];
        let mut compressor = Compress::new(Compression::fast(), false);
        let mut compressed = Vec::with_capacity(8192);
        loop {
            let consumed = usize::try_from(compressor.total_in())
                .unwrap_or(input.len())
                .min(input.len());
            compressor
                .compress_vec(&input[consumed..], &mut compressed, FlushCompress::Sync)
                .unwrap();
            let consumed = usize::try_from(compressor.total_in())
                .unwrap_or(input.len())
                .min(input.len());
            if consumed == input.len() && compressed.ends_with(&[0, 0, 255, 255]) {
                compressed.truncate(compressed.len() - 4);
                break;
            }
            compressed.reserve(8192);
        }

        let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
        assert_eq!(
            deflate.decompress(&compressed, 4 * 1024 * 1024),
            Err(DecompressionError::TooLarge)
        );

        let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
        assert_eq!(
            deflate.decompress(&[0xff], 4 * 1024 * 1024),
            Err(DecompressionError::Invalid)
        );
    }
}
