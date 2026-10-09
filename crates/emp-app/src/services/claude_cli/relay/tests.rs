use super::*;
use std::io::Write;
use std::net::Ipv4Addr;
use std::{thread, time::Duration};

#[test]
fn relay_reads_a_fragmented_request_from_a_nonblocking_accepted_socket() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind relay test");
    let address = listener.local_addr().expect("relay test address");
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).expect("connect relay test");
        stream
            .write_all(b"HEAD /api/hello HTTP/1.1\r\n")
            .expect("write first request fragment");
        thread::sleep(Duration::from_millis(40));
        stream
            .write_all(b"Host: localhost\r\n\r\n")
            .expect("write remaining request fragment");
    });

    let (mut accepted, _) = listener.accept().expect("accept relay test");
    accepted
        .set_nonblocking(true)
        .expect("make accepted relay socket nonblocking");
    let request = read_accepted_request(&mut accepted, "fixture", &Cancellation::new().unwrap())
        .expect("read fragmented relay request");

    assert_eq!(request.method, "HEAD");
    assert_eq!(request.path, "/api/hello");
    assert!(request.body.is_null());
    client.join().expect("fragmented relay client");
}
