use emp_test_support::{Deadline, test_timeout};
use std::{io, net::SocketAddr};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Duration, timeout},
};

pub fn wire_timeout() -> Duration {
    test_timeout(Duration::from_secs(5))
}

pub async fn head(io: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(io.read_u8().await?);
        assert!(bytes.len() < 32 * 1024);
    }
    Ok(bytes)
}

pub struct RawClient {
    pub io: TcpStream,
    pub head: String,
}

impl RawClient {
    pub async fn request(address: SocketAddr, request: &[u8]) -> Self {
        Self::request_before(address, request, &Deadline::after(wire_timeout())).await
    }

    pub async fn request_before(address: SocketAddr, request: &[u8], deadline: &Deadline) -> Self {
        let client = timeout(deadline.remaining(), async {
            let mut io = TcpStream::connect(address).await.unwrap();
            io.write_all(request).await.unwrap();
            let head = String::from_utf8(head(&mut io).await.unwrap()).unwrap();
            Self { io, head }
        })
        .await
        .expect("HTTP response headers before deadline");
        deadline.accept(client, "HTTP response headers").unwrap()
    }

    pub async fn rest(&mut self, deadline: &Deadline) -> Vec<u8> {
        let mut bytes = Vec::new();
        timeout(deadline.remaining(), self.io.read_to_end(&mut bytes))
            .await
            .expect("HTTP response ends before deadline")
            .unwrap();
        deadline.accept(bytes, "HTTP response EOF").unwrap()
    }
}

#[cfg(unix)]
pub fn websocket_frame(first: u8, mask: Option<[u8; 4]>, payload: &[u8]) -> Vec<u8> {
    let mut frame = vec![first];
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        length if length < 126 => frame.push(mask_bit | length as u8),
        length if length <= u16::MAX as usize => {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            frame.push(mask_bit | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    if let Some(mask) = mask {
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
    frame
}

#[cfg(unix)]
pub async fn read_websocket_frame(
    io: &mut (impl AsyncRead + Unpin),
    masked: bool,
    deadline: &Deadline,
) -> (u8, Vec<u8>) {
    let frame = timeout(deadline.remaining(), async {
        let first = io.read_u8().await.unwrap();
        let header = io.read_u8().await.unwrap();
        assert_eq!(header & 0x80 != 0, masked);
        let length = match header & 0x7f {
            126 => usize::from(io.read_u16().await.unwrap()),
            127 => usize::try_from(io.read_u64().await.unwrap()).unwrap(),
            length => usize::from(length),
        };
        assert!(length <= 256 * 1024 * 1024);
        let mut mask = [0; 4];
        if masked {
            io.read_exact(&mut mask).await.unwrap();
        }
        let mut payload = vec![0; length];
        io.read_exact(&mut payload).await.unwrap();
        if masked {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        (first, payload)
    })
    .await
    .expect("WebSocket frame before deadline");
    deadline.accept(frame, "WebSocket frame").unwrap()
}
