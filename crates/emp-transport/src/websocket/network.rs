//! TCP/TLS dialing and HTTP/SOCKS proxy handshakes.
use super::ClientWebSocketError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

pub(super) trait ReadWrite: Read + Write + Send {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
    fn readiness_stream(&self) -> std::io::Result<TcpStream> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
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
    fn readiness_stream(&self) -> std::io::Result<TcpStream> {
        self.try_clone()
    }
}
impl<S: ReadWrite> ReadWrite for rustls::StreamOwned<rustls::ClientConnection, S> {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.sock.set_read_timeout(timeout)
    }
    fn readiness_stream(&self) -> std::io::Result<TcpStream> {
        self.sock.readiness_stream()
    }
}
impl ReadWrite for Box<dyn ReadWrite> {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.as_ref().set_read_timeout(timeout)
    }
    fn readiness_stream(&self) -> std::io::Result<TcpStream> {
        self.as_ref().readiness_stream()
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

pub(super) fn read_http_head(stream: &mut dyn ReadWrite) -> Result<Vec<u8>, ClientWebSocketError> {
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
    let proxy_io = || ClientWebSocketError::new(503, "native websocket proxy failed");
    let credential = (!proxy.username().is_empty() || proxy.password().is_some())
        .then(|| (proxy.username(), proxy.password().unwrap_or_default()));
    let methods: &[u8] = if credential.is_some() { &[0, 2] } else { &[0] };
    stream
        .write_all(&[&[5, methods.len() as u8], methods].concat())
        .and_then(|_| stream.flush())
        .map_err(|_| proxy_io())?;
    let mut selected = [0_u8; 2];
    stream.read_exact(&mut selected).map_err(|_| proxy_io())?;
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
            .map_err(|_| proxy_io())?;
        let mut response = [0_u8; 2];
        stream.read_exact(&mut response).map_err(|_| proxy_io())?;
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
        .map_err(|_| proxy_io())?;
    let mut response = [0_u8; 4];
    stream.read_exact(&mut response).map_err(|_| proxy_io())?;
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
            stream.read_exact(&mut length).map_err(|_| proxy_io())?;
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
    stream.read_exact(&mut ignored).map_err(|_| proxy_io())
}

pub(super) fn websocket_connection(
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
