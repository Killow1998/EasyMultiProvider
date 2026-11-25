use super::*;
use crate::server::raw_tcp::{head, wire_timeout};
use emp_transport::websocket_accept;
use std::{net::Ipv4Addr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

async fn upstream_pair() -> (AsyncClientWebSocket, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let url = format!("ws://{}/sideband", listener.local_addr().unwrap());
    let empty = Default::default();
    let connect =
        AsyncClientWebSocket::connect_with_proxy(&url, &empty, wire_timeout().duration(), None);
    let upgrade = async {
        let (mut peer, _) = listener.accept().await.unwrap();
        let request = String::from_utf8(head(&mut peer).await.unwrap()).unwrap();
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .unwrap();
        let accept = websocket_accept(key).unwrap();
        peer.write_all(format!(
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n",
        ).as_bytes()).await.unwrap();
        peer
    };
    let (client, peer) = tokio::join!(connect, upgrade);
    (client.unwrap(), peer)
}

async fn read_masked_control(peer: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0; 6];
    peer.read_exact(&mut header).await.unwrap();
    assert_ne!(header[1] & 0x80, 0);
    let mut payload = vec![0; usize::from(header[1] & 0x7f)];
    peer.read_exact(&mut payload).await.unwrap();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= header[2 + index % 4];
    }
    (header[0], payload)
}

#[tokio::test]
async fn close_flushes_once_and_exits_with_a_saturated_event_queue() {
    timeout(wire_timeout().duration(), async {
        let (client, mut peer) = upstream_pair().await;
        let (commands, command_rx) = mpsc::channel(SIDEBAND_COMMAND_CAPACITY);
        let (events, event_rx) = mpsc::channel(1);
        assert!(
            events
                .try_send(UpstreamEvent::Text("queued".to_owned()))
                .is_ok()
        );
        assert_eq!(events.capacity(), 0);
        peer.write_all(b"\x89\x01p\x81\x05extra").await.unwrap();
        let task = tokio::spawn(relay_upstream(client, command_rx, events, |_, _| {
            panic!("a close command must not confirm a text write");
        }));
        assert_eq!(read_masked_control(&mut peer).await, (0x8a, b"p".to_vec()));

        timeout(Duration::from_secs(1), async {
            assert!(
                commands
                    .send(Command::Close(1001, "downstream closed"))
                    .await
                    .is_ok()
            );
            assert_eq!(
                read_masked_control(&mut peer).await,
                (
                    0x88,
                    [b"\x03\xe9".as_slice(), b"downstream closed"].concat()
                )
            );
            assert_eq!(peer.read(&mut [0]).await.unwrap(), 0, "one close frame");
            task.await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!(event_rx.len(), 1, "close did not wait for a consumer");
    })
    .await
    .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn command_queue_backpressures_and_reports_reset_for_only_the_active_write() {
    use std::sync::{Arc, Mutex};

    timeout(wire_timeout().duration(), async {
        let (mut client, mut peer) = upstream_pair().await;
        client.set_max_message_bytes(16 * 1024 * 1024).unwrap();
        let (commands, command_rx) = mpsc::channel(SIDEBAND_COMMAND_CAPACITY);
        let (events, mut event_rx) = mpsc::channel(SIDEBAND_EVENT_CAPACITY);
        let writes = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&writes);
        let task = tokio::spawn(relay_upstream(
            client,
            command_rx,
            events,
            move |id, result| {
                observed.lock().unwrap().push((id, result));
            },
        ));
        assert!(
            commands
                .send(Command::Text(1, "x".repeat(16 * 1024 * 1024)))
                .await
                .is_ok()
        );
        let mut header = [0; 14];
        peer.read_exact(&mut header).await.unwrap();
        assert_eq!(&header[..2], &[0x81, 0xff]);
        assert!(
            writes.lock().unwrap().is_empty(),
            "queue admission cannot confirm a write"
        );

        assert!(commands.try_send(Command::Text(2, "queued".into())).is_ok());
        assert!(matches!(
            commands.try_send(Command::Text(3, "retained".into())),
            Err(mpsc::error::TrySendError::Full(Command::Text(3, text))) if text == "retained"
        ));
        peer.set_zero_linger().unwrap();
        drop(peer);

        assert!(matches!(
            event_rx.recv().await,
            Some(UpstreamEvent::WriteFailed)
        ));
        task.await.unwrap();
        let trace = writes.lock().unwrap();
        assert_eq!(trace.len(), 2);
        assert!(matches!(&trace[0], (1, Some(Err(error))) if error.status() == 502));
        assert_eq!(trace[1], (2, None));
    })
    .await
    .unwrap();
}
