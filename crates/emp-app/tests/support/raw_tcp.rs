use std::{io, net::SocketAddr};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Duration, timeout},
};

pub const DEADLINE: Duration = Duration::from_secs(5);

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
        timeout(DEADLINE, async {
            let mut io = TcpStream::connect(address).await.unwrap();
            io.write_all(request).await.unwrap();
            let head = String::from_utf8(head(&mut io).await.unwrap()).unwrap();
            Self { io, head }
        })
        .await
        .expect("HTTP response headers before deadline")
    }

    pub async fn rest(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        timeout(DEADLINE, self.io.read_to_end(&mut bytes))
            .await
            .expect("HTTP response ends before deadline")
            .unwrap();
        bytes
    }
}
