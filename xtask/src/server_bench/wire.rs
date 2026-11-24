use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::time::Duration;

pub(super) const IO_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) type Socket = BufReader<TcpStream>;

pub(super) fn bind_high_port() -> io::Result<TcpListener> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    if listener.local_addr()?.port() > 4300 {
        Ok(listener)
    } else {
        Err(io::Error::other("OS assigned a loopback port <= 4300"))
    }
}

pub(super) fn socket(stream: TcpStream, timeout: Duration) -> io::Result<Socket> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(BufReader::new(stream))
}

pub(super) fn connect(port: u16, timeout: Duration) -> io::Result<Socket> {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into();
    socket(TcpStream::connect_timeout(&address, timeout)?, timeout)
}

pub(super) fn request(port: u16, wire: &str, timeout: Duration) -> io::Result<(Head, Socket)> {
    let mut socket = connect(port, timeout)?;
    socket.get_mut().write_all(wire.as_bytes())?;
    let head = Head::read(&mut socket)?.ok_or(io::ErrorKind::UnexpectedEof)?;
    Ok((head, socket))
}

pub(super) fn http_request(
    port: u16,
    method: &str,
    path: &str,
    header: &str,
    body: &str,
) -> String {
    format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{header}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

pub(super) struct Head {
    pub(super) line: String,
    headers: BTreeMap<String, String>,
}

impl Head {
    pub(super) fn read(socket: &mut Socket) -> io::Result<Option<Self>> {
        let mut line = String::new();
        if socket.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let mut headers = BTreeMap::new();
        loop {
            let mut field = String::new();
            socket.read_line(&mut field)?;
            if field == "\r\n" {
                return Ok(Some(Self { line, headers }));
            }
            let (name, value) = field
                .split_once(':')
                .ok_or(io::Error::other("invalid HTTP header"))?;
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub(super) fn status(&self) -> Option<u16> {
        self.line.split_whitespace().nth(1)?.parse().ok()
    }

    pub(super) fn expect_status(&self, expected: u16) -> io::Result<()> {
        if self.status() == Some(expected) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "expected HTTP {expected}, got {}",
                self.line.trim()
            )))
        }
    }

    pub(super) fn body(&self, socket: Socket) -> io::Result<Body> {
        let chunked = self.header("transfer-encoding") == Some("chunked");
        let remaining = if chunked {
            Some(0)
        } else {
            self.header("content-length")
                .map(str::parse)
                .transpose()
                .map_err(io::Error::other)?
        };
        Ok(Body {
            socket,
            chunked,
            remaining,
        })
    }
}

pub(super) struct Body {
    socket: Socket,
    chunked: bool,
    remaining: Option<usize>,
}

impl Read for Body {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.chunked && self.remaining == Some(0) {
            let mut line = String::new();
            self.socket.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim(), 16).map_err(io::Error::other)?;
            self.remaining = Some(size);
            if size == 0 {
                self.chunked = false;
            }
        }
        let Some(remaining) = self.remaining else {
            return self.socket.read(buffer);
        };
        let length = remaining.min(buffer.len());
        if length == 0 {
            return Ok(0);
        }
        let read = self.socket.read(&mut buffer[..length])?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining = Some(remaining - read);
        if self.chunked && self.remaining == Some(0) {
            let mut end = [0; 2];
            self.socket.read_exact(&mut end)?;
            if end != *b"\r\n" {
                return Err(io::Error::other("invalid HTTP chunk terminator"));
            }
        }
        Ok(read)
    }
}

pub(super) fn websocket_accept(key: &str) -> String {
    let challenge = format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    STANDARD.encode(
        ring::digest::digest(
            &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            challenge.as_bytes(),
        )
        .as_ref(),
    )
}

pub(super) fn write_frame(
    socket: &mut Socket,
    opcode: u8,
    payload: &[u8],
    masked: bool,
) -> io::Result<()> {
    let mut frame = vec![0x80 | opcode];
    let mask_bit = if masked { 0x80 } else { 0 };
    if payload.len() < 126 {
        frame.push(mask_bit | payload.len() as u8);
    } else {
        frame.push(mask_bit | 126);
        let size = u16::try_from(payload.len()).map_err(io::Error::other)?;
        frame.extend_from_slice(&size.to_be_bytes());
    }
    if masked {
        let mut mask = [0; 4];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut mask)
            .map_err(|_| io::Error::other("generate WebSocket mask"))?;
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
    } else {
        frame.extend_from_slice(payload);
    }
    socket.get_mut().write_all(&frame)
}

pub(super) fn read_frame(socket: &mut Socket, masked: bool) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0; 2];
    socket.read_exact(&mut head)?;
    let size = match head[1] & 0x7f {
        126 => {
            let mut bytes = [0; 2];
            socket.read_exact(&mut bytes)?;
            usize::from(u16::from_be_bytes(bytes))
        }
        127 => return Err(io::Error::other("benchmark WebSocket frame exceeds 64 KiB")),
        size => usize::from(size),
    };
    if head[0] & 0xf0 != 0x80 || (head[1] & 0x80 != 0) != masked {
        return Err(io::Error::other(
            "expected a final, uncompressed WebSocket frame with the peer's mask policy",
        ));
    }
    let mut mask = [0; 4];
    if masked {
        socket.read_exact(&mut mask)?;
    }
    let mut payload = vec![0; size];
    socket.read_exact(&mut payload)?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((head[0] & 0x0f, payload))
}
