//! Negotiated compression preserves streaming, decoded limits and explicit opt-out.
use emp_transport::{HttpClient, HttpClientPolicy, HttpMethod, HttpTransportErrorKind};
use flate2::{
    Compression,
    write::{GzEncoder, ZlibEncoder},
};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn encode_parts(encoding: &str, first: &[u8], last: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (prefix, encoded) = match encoding {
        "gzip" => {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(first).unwrap();
            encoder.flush().unwrap();
            let prefix = encoder.get_ref().clone();
            encoder.write_all(last).unwrap();
            (prefix, encoder.finish().unwrap())
        }
        "deflate" => {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(first).unwrap();
            encoder.flush().unwrap();
            let prefix = encoder.get_ref().clone();
            encoder.write_all(last).unwrap();
            (prefix, encoder.finish().unwrap())
        }
        "br" => {
            let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 4, 22);
            encoder.write_all(first).unwrap();
            encoder.flush().unwrap();
            let prefix = encoder.get_ref().clone();
            encoder.write_all(last).unwrap();
            (prefix, encoder.into_inner())
        }
        "zstd" => {
            let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
            encoder.write_all(first).unwrap();
            encoder.flush().unwrap();
            let prefix = encoder.get_ref().clone();
            encoder.write_all(last).unwrap();
            (prefix, encoder.finish().unwrap())
        }
        _ => unreachable!(),
    };
    let tail = encoded[prefix.len()..].to_vec();
    (prefix, tail)
}

fn receive_headers(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

#[tokio::test]
async fn all_negotiated_codecs_deliver_sse_before_upstream_finishes() {
    let first = format!(
        "data: {{\"delta\":\"{}\"}}\n\n",
        "streaming fixture ".repeat(2048)
    )
    .into_bytes();
    let last = b"data: [DONE]\n\n";
    for encoding in ["gzip", "br", "zstd", "deflate"] {
        let (prefix, tail) = encode_parts(encoding, &first, last);
        assert!(
            prefix.len() + tail.len() < first.len() / 4,
            "{encoding} did not reduce transferred bytes"
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (release, released) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let headers = receive_headers(&mut stream).to_ascii_lowercase();
            assert!(headers.contains("accept-encoding: gzip, br, zstd, deflate\r\n"));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", prefix.len()+tail.len()).unwrap();
            stream.write_all(&prefix).unwrap();
            stream.flush().unwrap();
            if released.recv_timeout(Duration::from_secs(5)).is_ok() {
                stream.write_all(&tail).unwrap();
                stream.flush().unwrap();
            }
        });
        let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
        let mut response = client
            .open(
                HttpMethod::Get,
                &format!("http://{address}/stream"),
                BTreeMap::new(),
                None,
                true,
            )
            .await
            .unwrap();
        assert!(response.header("content-encoding").is_none());
        assert!(
            response.header("content-length").is_none(),
            "encoded length must not describe decoded bytes"
        );
        let mut output = Vec::new();
        while output.len() < first.len() {
            output.extend(
                tokio::time::timeout(Duration::from_secs(2), response.next_chunk())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(output, first, "{encoding} changed the first event");
        release.send(()).unwrap();
        while let Some(chunk) = response.next_chunk().await.unwrap() {
            output.extend(chunk);
        }
        assert_eq!(&output[first.len()..], last);
        worker.join().unwrap();
    }
}

#[tokio::test]
async fn decompressed_body_limits_and_corrupt_streams_fail_closed() {
    for encoding in ["gzip", "br", "zstd", "deflate"] {
        let raw = b"compression expansion ".repeat(4096);
        let (prefix, tail) = encode_parts(encoding, &raw, &[]);
        let encoded = [prefix, tail].concat();
        for corrupt in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let mut body = encoded.clone();
            if corrupt {
                body.truncate(body.len() / 2);
            }
            let worker = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                receive_headers(&mut stream);
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Encoding: {encoding}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                let _ = stream.write_all(&body);
            });
            let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
            let response = client
                .open(
                    HttpMethod::Get,
                    &format!("http://{address}/body"),
                    BTreeMap::new(),
                    None,
                    false,
                )
                .await
                .unwrap();
            let result = response
                .read_limited(if corrupt { raw.len() * 2 } else { 128 })
                .await;
            assert!(
                result.is_err(),
                "{encoding}: corrupt={corrupt} was accepted"
            );
            if !corrupt {
                assert_eq!(
                    result.unwrap_err().kind(),
                    HttpTransportErrorKind::ResponseTooLarge
                );
            }
            worker.join().unwrap();
        }
    }
}

#[tokio::test]
async fn explicit_identity_is_preserved_and_plain_responses_remain_valid() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(
            receive_headers(&mut stream)
                .to_ascii_lowercase()
                .contains("accept-encoding: identity\r\n")
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });
    let client = HttpClient::new(HttpClientPolicy::default()).unwrap();
    let response = client
        .open(
            HttpMethod::Get,
            &format!("http://{address}/plain"),
            BTreeMap::from([("Accept-Encoding".into(), "identity".into())]),
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(response.read_limited(2).await.unwrap(), b"ok");
    worker.join().unwrap();
}
