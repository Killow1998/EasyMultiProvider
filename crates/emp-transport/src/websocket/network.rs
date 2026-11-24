use super::ClientWebSocketError;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::sync::Arc;
use std::time::Duration;
use std::{future::Future, io};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpStream, lookup_host};
use tokio_rustls::TlsConnector;
use url::Url;

mod handshake;
pub(super) use handshake::HandshakeIo;

pub(super) trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}
type OpenedWebSocketTransport = (Box<dyn Socket>, bool, Option<String>);

pub(super) async fn io_timeout<T>(
    timeout: Duration,
    io: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout(timeout, io)
        .await
        .map_err(|_| io::ErrorKind::TimedOut)?
}

async fn connect_tcp(
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<TcpStream, ClientWebSocketError> {
    let failure = || ClientWebSocketError::new(503, "native upstream websocket connection failed");
    let addresses = lookup_host((host, port)).await.map_err(|_| failure())?;
    for address in addresses {
        if let Ok(tcp) = io_timeout(timeout, TcpStream::connect(address)).await {
            return Ok(tcp);
        }
    }
    Err(failure())
}

async fn tls_stream(
    stream: Box<dyn Socket>,
    host: &str,
    timeout: Duration,
    handshake_error: &'static str,
) -> Result<Box<dyn Socket>, ClientWebSocketError> {
    let loaded = tokio::task::spawn_blocking(rustls_native_certs::load_native_certs)
        .await
        .map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket TLS setup failed")
        })?;
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
    let connector = TlsConnector::from(Arc::new(config));
    let mut stream = connector
        .connect(server_name, HandshakeIo::new(stream, timeout))
        .await
        .map_err(|_| ClientWebSocketError::new(503, handshake_error))?;
    stream.get_mut().0.finish();
    Ok(Box::new(stream))
}

pub(super) async fn read_http_head(
    stream: &mut (impl AsyncRead + Unpin),
) -> Result<Vec<u8>, ClientWebSocketError> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= 64 * 1024 {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket handshake is too large",
            ));
        }
        let byte = stream.read_u8().await.map_err(|_| {
            ClientWebSocketError::new(503, "native upstream websocket handshake failed")
        })?;
        head.push(byte);
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

async fn socks_connect(
    stream: &mut dyn Socket,
    proxy: &Url,
    host: &str,
    port: u16,
    timeout: Duration,
) -> Result<(), ClientWebSocketError> {
    let mut stream = HandshakeIo::new(stream, timeout);
    let proxy_io = || ClientWebSocketError::new(503, "native websocket proxy failed");
    let credential = (!proxy.username().is_empty() || proxy.password().is_some())
        .then(|| (proxy.username(), proxy.password().unwrap_or_default()));
    let methods: &[u8] = if credential.is_some() { &[0, 2] } else { &[0] };
    stream
        .write_flushed(&[&[5, methods.len() as u8], methods].concat())
        .await
        .map_err(|_| proxy_io())?;
    let mut selected = [0_u8; 2];
    stream
        .read_exact(&mut selected)
        .await
        .map_err(|_| proxy_io())?;
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
            .write_flushed(&request)
            .await
            .map_err(|_| proxy_io())?;
        let mut response = [0_u8; 2];
        stream
            .read_exact(&mut response)
            .await
            .map_err(|_| proxy_io())?;
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
        .write_flushed(&request)
        .await
        .map_err(|_| proxy_io())?;
    let mut response = [0_u8; 4];
    stream
        .read_exact(&mut response)
        .await
        .map_err(|_| proxy_io())?;
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
                .await
                .map_err(|_| proxy_io())?;
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
        .await
        .map(|_| ())
        .map_err(|_| proxy_io())
}

pub(super) async fn websocket_connection(
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
        let tcp = connect_tcp(host, port, timeout).await?;
        let stream: Box<dyn Socket> = Box::new(tcp);
        return Ok((
            if target.scheme() == "wss" {
                tls_stream(
                    stream,
                    host,
                    timeout,
                    "native upstream websocket handshake failed",
                )
                .await?
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
    let tcp = connect_tcp(proxy_host, proxy_port, timeout).await?;
    let mut stream: Box<dyn Socket> = Box::new(tcp);
    if proxy.scheme() == "https" {
        stream = tls_stream(
            stream,
            proxy_host,
            timeout,
            if target.scheme() == "wss" {
                "native websocket proxy failed"
            } else {
                "native upstream websocket handshake failed"
            },
        )
        .await?;
    }
    if matches!(proxy.scheme(), "socks5" | "socks5h") {
        socks_connect(stream.as_mut(), &proxy, host, port, timeout).await?;
        if target.scheme() == "wss" {
            stream = tls_stream(
                stream,
                host,
                timeout,
                "native upstream websocket handshake failed",
            )
            .await?;
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
        let request = format!(
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{auth}Connection: keep-alive\r\n\r\n"
        );
        let mut handshake = HandshakeIo::new(stream.as_mut(), timeout);
        handshake
            .write_flushed(request.as_bytes())
            .await
            .map_err(|_| ClientWebSocketError::new(503, "native websocket proxy failed"))?;
        let head = read_http_head(&mut handshake).await?;
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
        stream = tls_stream(
            stream,
            host,
            timeout,
            "native upstream websocket handshake failed",
        )
        .await?;
        Ok((stream, false, None))
    } else {
        Ok((stream, true, authorization))
    }
}
