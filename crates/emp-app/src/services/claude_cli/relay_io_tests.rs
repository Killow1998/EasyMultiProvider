use super::*;
use std::net::{Ipv4Addr, TcpListener};
use std::sync::{Arc, mpsc};
use std::thread;

fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

#[test]
fn cancellation_wakes_idle_accept_and_remains_visible_to_late_waiters() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let cancelled = Arc::new(Cancellation::new().unwrap());
    let worker_cancelled = Arc::clone(&cancelled);
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = accept(&listener, &worker_cancelled);
        done_tx
            .send(matches!(result, Err(ClaudeCliError::Disconnected)))
            .unwrap();
    });
    thread::sleep(Duration::from_millis(20));
    cancelled.cancel();
    let result = done_rx.recv_timeout(Duration::from_secs(2));
    // Release a faulty accept loop before asserting, so a regression leaves
    // no detached listener behind in the test process.
    if result.is_err() {
        let _ = TcpStream::connect(address);
    }
    worker.join().unwrap();
    assert_eq!(result, Ok(true));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), cancelled.cancelled())
            .await
            .unwrap();
    });
}

#[test]
fn unauthorized_request_is_rejected_without_waiting_for_its_body() {
    let (mut client, mut server) = socket_pair();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client.write_all(b"POST /v1/messages HTTP/1.1\r\nAuthorization: Bearer wrong\r\nContent-Length: 1000\r\n\r\n").unwrap();
    let worker = thread::spawn(move || {
        read_accepted_request(&mut server, "fixture", &Cancellation::new().unwrap())
    });
    let mut reply = String::new();
    let received = client.read_to_string(&mut reply);
    // Close before asserting so a failing reader is never left running.
    drop(client);
    let result = worker.join().unwrap();
    received.expect("the 401 must arrive while the peer still withholds the body");
    assert!(reply.starts_with("HTTP/1.1 401 "));
    assert!(matches!(
        result,
        Err(ClaudeCliError::Failure("claude_cli_relay_auth_failed"))
    ));
}

#[test]
fn cancellation_interrupts_headers_and_authenticated_body_reads() {
    for prefix in [
        "POST /v1/messages HTTP/1.1\r\n",
        "POST /v1/messages HTTP/1.1\r\nAuthorization: Bearer fixture\r\nContent-Length: 1000\r\n\r\n{",
    ] {
        let (mut client, mut server) = socket_pair();
        client.write_all(prefix.as_bytes()).unwrap();
        let cancelled = Arc::new(Cancellation::new().unwrap());
        let worker_cancelled = Arc::clone(&cancelled);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let result = read_accepted_request(&mut server, "fixture", &worker_cancelled);
            done_tx
                .send(matches!(result, Err(ClaudeCliError::Disconnected)))
                .unwrap();
        });
        ready_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(150));
        cancelled.cancel();
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        drop(client);
        worker.join().unwrap();
        assert_eq!(
            result,
            Ok(true),
            "cancellation must not wait for the peer to close"
        );
    }
}

#[test]
fn ongoing_input_does_not_extend_the_absolute_deadline() {
    let (mut client, mut server) = socket_pair();
    let cancelled = Cancellation::new().unwrap();
    let writer = thread::spawn(move || {
        // Every individual read can make progress, but the overall operation
        // still has one deadline, not a fresh timeout for every byte.
        for _ in 0..100 {
            if client.write_all(b"x").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let started = Instant::now();
    let result = {
        let mut stream = RelayIo::new(&mut server, &cancelled).unwrap();
        stream.deadline = Instant::now() + Duration::from_millis(150);
        stream.read_exact(&mut [0; 100])
    };
    let elapsed = started.elapsed();
    drop(server);
    writer.join().unwrap();
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(elapsed < Duration::from_secs(1));
}

#[test]
fn cancellation_interrupts_a_response_to_a_peer_that_stops_reading() {
    let (client, mut server) = socket_pair();
    let cancelled = Arc::new(Cancellation::new().unwrap());
    let worker_cancelled = Arc::clone(&cancelled);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let bytes = vec![b'x'; 16 * 1024 * 1024];
        ready_tx.send(()).unwrap();
        let result = write_bytes(&mut server, &bytes, &worker_cancelled);
        done_tx
            .send(matches!(result, Err(ClaudeCliError::Disconnected)))
            .unwrap();
    });
    ready_rx.recv().unwrap();
    thread::sleep(Duration::from_millis(150));
    cancelled.cancel();
    let result = done_rx.recv_timeout(Duration::from_secs(2));
    drop(client);
    worker.join().unwrap();
    assert_eq!(
        result,
        Ok(true),
        "a non-reading client must not hold up relay cleanup"
    );
}

#[test]
fn authorized_fragmented_request_preserves_body_and_protocol_headers() {
    let (mut client, mut server) = socket_pair();
    let writer = thread::spawn(move || {
        client.write_all(b"POST /v1/messages?beta=true HTTP/1.1\r\nAuthorization: Bearer fixture\r\nContent-Length: 17\r\nAnthropic-Version: 2023-06-01\r\nAnthropic-Beta: fixture-beta\r\n\r\n{\"messages\":").unwrap();
        thread::sleep(Duration::from_millis(20));
        client.write_all(b"[{}]}").unwrap();
    });
    let request =
        read_accepted_request(&mut server, "fixture", &Cancellation::new().unwrap()).unwrap();
    writer.join().unwrap();
    assert_eq!(request.body, serde_json::json!({"messages":[{}]}));
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(request.protocol_headers["anthropic-beta"], "fixture-beta");
    assert_eq!(request.protocol_headers["anthropic-version"], "2023-06-01");
}
