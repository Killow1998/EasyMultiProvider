#[allow(dead_code, reason = "This target uses only the HTTP parsing fixtures")]
mod support;

use emp_test_support::{Deadline, test_timeout};
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    time::Duration,
};
use support::http::{Response, SseClient, read_message};

#[test]
fn empty_and_truncated_responses_return_errors() {
    for response in [
        b"".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nx",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n0\r\n",
    ] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server.write_all(response).unwrap();
        drop(server);
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        assert!(
            matches!(
                Response::read_before(client, &deadline),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof
            ),
            "{response:?}"
        );
    }
}

#[test]
fn fixed_length_response_finishes_while_peer_is_open() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    server
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
        .unwrap();

    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let response = Response::read_before(client, &deadline).unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"{}");
    drop(server);
}

#[test]
fn both_chunk_readers_reject_invalid_trailer_fields() {
    let head =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n";
    let body = b"0\r\ngarbage\r\n\r\n";
    assert_eq!(
        read_message(&mut head.as_slice().chain(body.as_slice()), true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    server.write_all(head).unwrap();
    server.write_all(body).unwrap();

    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let mut reader = SseClient::read_head(client, &deadline);
    assert_eq!(
        reader.expect_eof(&deadline).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    reader.abandon("fixture deliberately sends an invalid chunk terminator");
}

#[test]
fn chunked_sse_requires_the_final_empty_line_even_after_completion() {
    for complete in [true, false] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let event = b"data: {\"type\":\"response.completed\"}\n\n";
        write!(
            server,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            event.len()
        )
        .unwrap();
        server.write_all(event).unwrap();
        server.write_all(b"\r\n0\r\n").unwrap();
        if complete {
            server.write_all(b"\r\n").unwrap();
        }

        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        let mut client = SseClient::read_head(client, &deadline);
        assert_eq!(
            client.next_event(&deadline).unwrap().unwrap()["type"],
            "response.completed"
        );
        let deadline = if complete {
            deadline
        } else {
            Deadline::after(test_timeout(Duration::from_millis(100)))
        };
        let eof = client.expect_eof(&deadline);
        if complete {
            eof.unwrap();
        } else {
            assert_eq!(eof.unwrap_err().kind(), io::ErrorKind::TimedOut);
            client
                .abandon("fixture keeps the socket open without the terminator's final empty line");
        }
        drop(server);
    }
}

#[test]
fn done_marker_leaves_its_chunk_ending_for_expect_eof() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    server
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\ne\r\ndata: [DONE]\n\n",
        )
        .unwrap();
    drop(server);

    let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
    let mut client = SseClient::read_head(client, &deadline);
    assert!(client.next_event(&deadline).unwrap().is_none());
    assert_eq!(
        client.expect_eof(&deadline).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
    client.abandon(
        "fixture deliberately closes after the done frame without sending the chunk ending",
    );
}

#[test]
fn rejects_conflicting_message_framing() {
    for response in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\ncontent-length: 0\r\n\r\n",
    ] {
        let mut input = response;
        assert_eq!(
            read_message(&mut input, true).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "{response:?}"
        );
    }
}
