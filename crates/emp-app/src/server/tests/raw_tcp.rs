use crate::server::raw_tcp::{read_websocket_frame, wire_timeout};
use emp_test_support::{Deadline, test_timeout};
use tokio::{io::AsyncWriteExt, time::Duration};

#[tokio::test(start_paused = true)]
#[should_panic(expected = "WebSocket frame before deadline")]
async fn stalled_payload_expires_before_the_turn_deadline() {
    let (mut client, mut server) = tokio::io::duplex(64);
    let delay = wire_timeout() + test_timeout(Duration::from_secs(1));
    let deadline = Deadline::after(delay + delay + wire_timeout());
    tokio::join!(read_websocket_frame(&mut client, false, &deadline), async {
        tokio::time::sleep(delay.duration()).await;
        server.write_all(&[0x81, 1]).await.unwrap();
        tokio::time::sleep(delay.duration()).await;
        server.write_all(b"x").await.unwrap();
    });
}
