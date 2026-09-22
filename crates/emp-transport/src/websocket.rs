use crate::MAX_PROXY_REQUEST_BYTES;
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

    fn decompress(&mut self, payload: &[u8]) -> Result<Vec<u8>, ClientWebSocketError> {
        let mut encoded = Vec::with_capacity(payload.len() + 4);
        encoded.extend_from_slice(payload);
        encoded.extend_from_slice(&[0, 0, 255, 255]);
        let before_in = self.decompressor.total_in();
        let mut output = Vec::with_capacity(encoded.len().saturating_mul(2).max(8192));
        loop {
            output.reserve(8192);
            let consumed = usize::try_from(self.decompressor.total_in() - before_in)
                .unwrap_or(encoded.len())
                .min(encoded.len());
            self.decompressor
                .decompress_vec(&encoded[consumed..], &mut output, FlushDecompress::Sync)
                .map_err(|_| {
                    ClientWebSocketError::new(
                        502,
                        "native upstream websocket compression is invalid",
                    )
                })?;
            if output.len() > MAX_PROXY_REQUEST_BYTES {
                return Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket event is too large",
                ));
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
    compression: Option<PerMessageDeflate>,
    max_message_bytes: usize,
    local_control: bool,
}

impl ClientWebSocket {
    pub fn connect(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        Self::connect_with_proxy(url, headers, timeout, None)
    }

    pub fn connect_with_proxy(
        url: &str,
        headers: &std::collections::BTreeMap<String, String>,
        timeout: Duration,
        proxy: Option<&str>,
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
        let (stream, absolute_form, proxy_authorization) =
            websocket_connection(&parsed, proxy, timeout)?;
        Self::handshake(
            stream,
            &parsed,
            headers,
            absolute_form,
            proxy_authorization,
            true,
            Duration::from_secs(300),
        )
    }

    /// Read-only Codex control transport. It never consults proxy settings.
    pub fn connect_local(
        path: &std::path::Path,
        timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        std::fs::metadata(path).map_err(local_socket_error)?;
        let address = socket2::SockAddr::unix(path).map_err(local_socket_error)?;
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)
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
        let url = Url::parse("ws://localhost/").expect("constant local WebSocket URL");
        let mut client = Self::handshake(
            Box::new(socket),
            &url,
            &std::collections::BTreeMap::new(),
            false,
            None,
            false,
            timeout,
        )?;
        client.max_message_bytes = 2 * 1024 * 1024;
        client.local_control = true;
        Ok(client)
    }

    fn handshake(
        mut stream: Box<dyn ReadWrite>,
        parsed: &Url,
        headers: &std::collections::BTreeMap<String, String>,
        absolute_form: bool,
        proxy_authorization: Option<String>,
        negotiate_compression: bool,
        read_timeout: Duration,
    ) -> Result<Self, ClientWebSocketError> {
        let host = parsed.host_str().ok_or_else(|| {
            ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
        })?;
        let port = parsed.port_or_known_default().ok_or_else(|| {
            ClientWebSocketError::new(502, "native upstream websocket endpoint is invalid")
        })?;
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
        if absolute_form {
            path = format!("{}://{authority}{path}", parsed.scheme());
        }
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
        );
        if negotiate_compression {
            request.push_str(
                "Sec-WebSocket-Extensions: permessage-deflate; client_max_window_bits\r\n",
            );
        }
        if let Some(authorization) = proxy_authorization {
            request.push_str("Proxy-Authorization: ");
            request.push_str(&authorization);
            request.push_str("\r\n");
        }
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
        let compression = match response_headers.get("sec-websocket-extensions") {
            Some(_) if !negotiate_compression => {
                return Err(ClientWebSocketError::new(
                    502,
                    "local control websocket returned an unsolicited extension",
                ));
            }
            Some(value) => Some(PerMessageDeflate::negotiated(value)?),
            None => None,
        };
        stream.set_read_timeout(Some(read_timeout)).map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket timeout setup failed")
        })?;
        Ok(Self {
            stream,
            closed: false,
            response_headers,
            compression,
            max_message_bytes: MAX_PROXY_REQUEST_BYTES,
            local_control: false,
        })
    }
    pub fn response_headers(&self) -> &std::collections::BTreeMap<String, String> {
        &self.response_headers
    }
    fn read_exact(&mut self, length: usize) -> Result<Vec<u8>, ClientWebSocketError> {
        let mut value = vec![0u8; length];
        self.stream.read_exact(&mut value).map_err(|error| {
            if self.local_control {
                local_socket_error(error)
            } else {
                ClientWebSocketError::new(502, "native upstream websocket closed")
            }
        })?;
        Ok(value)
    }
    fn send_frame(
        &mut self,
        opcode: u8,
        payload: &[u8],
        compressed: bool,
    ) -> Result<(), ClientWebSocketError> {
        let mut mask = [0u8; 4];
        getrandom::getrandom(&mut mask).map_err(|_| {
            ClientWebSocketError::new(500, "native websocket randomness is unavailable")
        })?;
        let mut frame = vec![0x80 | if compressed { 0x40 } else { 0 } | opcode];
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
        if payload.len() > MAX_PROXY_REQUEST_BYTES {
            return Err(ClientWebSocketError::new(
                413,
                "native websocket request is too large",
            ));
        }
        if let Some(compression) = self.compression.as_mut() {
            let payload = compression.compress(&payload)?;
            self.send_frame(1, &payload, true)
        } else {
            self.send_frame(1, &payload, false)
        }
    }
    pub fn receive_json(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        loop {
            match self.receive_value()? {
                Some(value) if !value.is_object() => continue,
                value => return Ok(value),
            }
        }
    }

    /// Control RPC skips unrelated JSON values; native Responses requires objects.
    pub fn receive_value(&mut self) -> Result<Option<Value>, ClientWebSocketError> {
        let mut message = Vec::new();
        let mut started = false;
        let mut compressed = false;
        loop {
            let header = self.read_exact(2)?;
            let final_frame = header[0] & 0x80 != 0;
            let rsv1 = header[0] & 0x40 != 0;
            if header[0] & 0x30 != 0 {
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
            if opcode >= 8 && (!final_frame || length > 125 || rsv1) {
                return Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket control frame is invalid",
                ));
            }
            let length = usize::try_from(length).map_err(|_| {
                ClientWebSocketError::new(502, "native upstream websocket event is too large")
            })?;
            if opcode < 8
                && message
                    .len()
                    .checked_add(length)
                    .is_none_or(|size| size > self.max_message_bytes)
            {
                return Err(ClientWebSocketError::new(
                    502,
                    "native upstream websocket event is too large",
                ));
            }
            let payload = self.read_exact(length)?;
            match opcode {
                1 | 2 => {
                    if started {
                        return Err(ClientWebSocketError::new(
                            502,
                            "native upstream websocket frame sequence is invalid",
                        ));
                    }
                    if rsv1 && self.compression.is_none() {
                        return Err(ClientWebSocketError::new(
                            502,
                            "native upstream websocket extension is unsupported",
                        ));
                    }
                    started = true;
                    compressed = rsv1;
                    message.extend_from_slice(&payload);
                }
                0 => {
                    if !started || rsv1 {
                        return Err(ClientWebSocketError::new(
                            502,
                            "native upstream websocket frame sequence is invalid",
                        ));
                    }
                    message.extend_from_slice(&payload);
                }
                8 => {
                    self.closed = true;
                    return Ok(None);
                }
                9 => {
                    self.send_frame(10, &payload, false)?;
                }
                10 => {}
                _ => {
                    return Err(ClientWebSocketError::new(
                        502,
                        "native upstream websocket frame type is unsupported",
                    ));
                }
            }
            if final_frame && started {
                if compressed {
                    message = self
                        .compression
                        .as_mut()
                        .expect("RSV1 requires negotiated compression")
                        .decompress(&message)?;
                }
                let text = std::str::from_utf8(&message).map_err(|_| {
                    ClientWebSocketError::new(502, "native upstream websocket event is not UTF-8")
                })?;
                match serde_json::from_str(text) {
                    Ok(value) => return Ok(Some(value)),
                    Err(_) if !self.local_control => {
                        message.clear();
                        started = false;
                        compressed = false;
                    }
                    Err(_) => {
                        return Err(ClientWebSocketError::new(
                            502,
                            "local control websocket event is invalid JSON",
                        ));
                    }
                }
            }
        }
    }
    pub fn close(&mut self) {
        if !self.closed {
            let _ = self.send_frame(8, &1000u16.to_be_bytes(), false);
            self.closed = true;
        }
    }
}
impl Drop for ClientWebSocket {
    fn drop(&mut self) {
        self.close();
    }
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
