use super::compression::{DecompressionError, PerMessageDeflate};
use super::frame::{FrameDecoder, FramePoll};
use super::*;
use crate::MAX_PROXY_REQUEST_BYTES;
use flate2::{Compress, Compression, FlushCompress};
use std::future::Future;
use std::io::Cursor;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, duplex};
use tokio::time::timeout;

fn client_socket() -> AsyncClientWebSocket {
    AsyncClientWebSocket::new(
        Box::new(tokio::io::join(tokio::io::empty(), tokio::io::sink())),
        Default::default(),
        None,
        Duration::from_secs(5),
    )
}

#[tokio::test]
async fn upstream_masked_frames_roundtrip_across_write_chunks() {
    for compressed in [false, true] {
        let (peer, mut socket) = duplex(8192);
        let mut client = client_socket();
        client.stream = Box::new(peer);
        let parameters =
            "permessage-deflate; client_no_context_takeover; server_no_context_takeover";
        if compressed {
            client.compression = Some(PerMessageDeflate::negotiated(parameters).unwrap());
        }
        let text = (0..200_000)
            .map(|index| char::from(b'!' + ((index * 17 + index / 37) % 90) as u8))
            .collect::<String>();
        let lengths = [0, 125, 126, 65_535, 65_536, 196_613];
        let write = async {
            for length in lengths {
                client.send_text(&text[..length]).await.unwrap();
            }
        };
        let read = async {
            let mut decoder = FrameDecoder::new(MAX_PROXY_REQUEST_BYTES);
            let mut codec = PerMessageDeflate::negotiated(parameters).unwrap();
            for length in lengths {
                match decoder.read(&mut socket, true, compressed).await.unwrap() {
                    FramePoll::Message {
                        payload,
                        compressed,
                        ..
                    } => {
                        let decoded = if compressed {
                            codec.decompress(&payload, MAX_PROXY_REQUEST_BYTES).unwrap()
                        } else {
                            payload
                        };
                        assert_eq!(decoded, &text.as_bytes()[..length]);
                    }
                    _ => panic!("expected complete masked text frame"),
                }
            }
        };
        timeout(Duration::from_secs(5), async { tokio::join!(write, read) })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn text_traffic_measures_encoding_excludes_control_and_keeps_unknown_partial_reads() {
    let mut client = client_socket();
    client.send_text("plain").await.unwrap();
    client.send_pong(b"ping").await.unwrap();
    assert_eq!(client.traffic().decoded_sent, Some(5));
    assert_eq!(client.traffic().encoded_sent, Some(5));

    let parameters = "permessage-deflate; client_no_context_takeover; server_no_context_takeover";
    let mut codec = PerMessageDeflate::negotiated(parameters).unwrap();
    let text = "synthetic compressed text ".repeat(256);
    let encoded = codec
        .compress(text.as_bytes(), MAX_PROXY_REQUEST_BYTES)
        .unwrap();
    let mut frame = vec![0xc1];
    frame.extend(frame_length_prefix(encoded.len()));
    frame.extend(&encoded);
    let mut compressed = client_socket();
    compressed.compression = Some(PerMessageDeflate::negotiated(parameters).unwrap());
    compressed.frame_decoder.feed(&frame).unwrap();
    assert_eq!(
        compressed.read().await.unwrap(),
        WebSocketEvent::Text(text.clone())
    );
    assert_eq!(
        compressed.traffic().decoded_received,
        Some(text.len() as u64)
    );
    assert_eq!(
        compressed.traffic().encoded_received,
        Some(encoded.len() as u64)
    );
    compressed.send_text(&text).await.unwrap();
    assert_eq!(compressed.traffic().decoded_sent, Some(text.len() as u64));
    assert_eq!(
        compressed.traffic().encoded_sent,
        Some(encoded.len() as u64)
    );

    let mut broken = client_socket();
    // Invalid unmasked-server frame: the rejected payload's length is unknown.
    broken
        .frame_decoder
        .feed(b"\x81\x82\x00\x00\x00\x00{}")
        .unwrap();
    assert!(broken.read().await.is_err());
    assert_eq!(broken.traffic().decoded_received, None);
    assert_eq!(broken.traffic().encoded_received, None);
}

fn masked_data_header(length: u64) -> Vec<u8> {
    let mut bytes = vec![0x81, 0xff];
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 4]);
    bytes
}

fn masked_text_frame(text: &str) -> Vec<u8> {
    let mask = [0x11, 0x22, 0x33, 0x44];
    let bytes = text.as_bytes();
    let mut frame = vec![0x81, 0x80 | bytes.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(
        bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    frame
}

#[tokio::test]
async fn async_reads_keep_partial_frames_and_allow_concurrent_large_write() {
    let (mut peer, socket) = duplex(8192);
    let (mut reader, writer) = split_websocket(socket, &[], MAX_PROXY_REQUEST_BYTES, None).unwrap();
    let frame = masked_text_frame("partial request");
    assert_pending(reader.read()).await;
    peer.write_all(&frame[..3]).await.unwrap();
    assert_pending(reader.read()).await;
    peer.write_all(&frame[3..]).await.unwrap();
    assert_eq!(
        reader.read().await.unwrap(),
        WebSocketEvent::Text("partial request".into())
    );

    let mut client = client_socket();
    client.stream = Box::new(peer);
    assert_pending(client.read()).await;
    let response = serde_json::json!({"text": "x".repeat(1024 * 1024)});
    let (written, received) = timeout(Duration::from_secs(5), async {
        tokio::join!(writer.send_json(&response), client.read())
    })
    .await
    .unwrap();
    written.unwrap();
    let WebSocketEvent::Text(text) = received.unwrap() else {
        panic!("expected the large response frame");
    };
    let response: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(response["text"].as_str().unwrap().len(), 1024 * 1024);
}

async fn assert_pending<T>(future: impl Future<Output = T>) {
    use std::task::Poll;
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[test]
fn regular_websockets_keep_legacy_cap_while_sideband_can_use_four_mib() {
    const FOUR_MIB: usize = 4 * 1024 * 1024;
    let declared_length = (FOUR_MIB + 1) as u64;

    let mut legacy_bytes = Cursor::new(masked_data_header(declared_length));
    let mut legacy = WebSocketConnection::new(&mut legacy_bytes);
    assert!(
        matches!(
            legacy.poll_text().unwrap(),
            WebSocketPoll::Closed { code: None }
        ),
        "the legacy cap accepts a 4 MiB+1 declaration"
    );

    let mut sideband_bytes = Cursor::new(masked_data_header(declared_length));
    let mut sideband = WebSocketConnection::new(&mut sideband_bytes);
    sideband.set_max_message_bytes(FOUR_MIB).unwrap();
    assert_eq!(sideband.poll_text().unwrap_err().close_code(), 1009);

    let client = client_socket();
    assert_eq!(client.max_message_bytes, MAX_PROXY_REQUEST_BYTES);
    assert_eq!(DEFAULT_WEBSOCKET_MESSAGE_BYTES, FOUR_MIB);
}

#[tokio::test]
async fn responses_reject_excess_length_and_concurrency_from_the_frame_header() {
    let limits = crate::RequestLimits::new(
        crate::RequestLimitsConfig::default(),
        || Some(crate::MemoryStatus::available(2 * 1024 * 1024 * 1024)),
        || 0,
        "websocket-capacity",
    )
    .unwrap();
    let maximum = MAX_RESPONSES_WEBSOCKET_BYTES;
    let open = || {
        let (peer, io) = duplex(64);
        let (reader, _) = split_websocket(io, &[], maximum, Some(limits.clone())).unwrap();
        (peer, reader)
    };

    let (mut peer, mut socket) = open();
    peer.write_all(&masked_data_header(maximum as u64 + 1))
        .await
        .unwrap();
    assert_eq!(socket.read().await.unwrap_err().close_code(), 1009);
    assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);

    let (mut first_peer, mut first) = open();
    first_peer
        .write_all(&masked_data_header(maximum as u64))
        .await
        .unwrap();
    assert_pending(first.read()).await;
    let reservation = first.take_request_budget().unwrap();
    assert_eq!(reservation.limit(), maximum);

    let (mut second_peer, mut second) = open();
    second_peer
        .write_all(&masked_data_header(maximum as u64))
        .await
        .unwrap();
    assert_eq!(second.read().await.unwrap_err().close_code(), 1013);
    drop(reservation);
    assert_pending(second.read()).await;
    assert!(limits.snapshot().unwrap().reserved_bytes > 0);
    drop(second_peer);
    assert!(matches!(
        second.read().await.unwrap(),
        WebSocketEvent::Closed { .. }
    ));
    drop(second);
    assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);
}

#[test]
fn downstream_upgrade_prefix_preserves_first_frame() {
    let prefix = masked_text_frame("first-frame");
    let mut stream = Cursor::new(Vec::new());
    let mut connection = WebSocketConnection::new_with_prefix(&mut stream, &prefix).unwrap();
    assert!(matches!(
        connection.poll_text().unwrap(),
        WebSocketPoll::Text(text) if text == "first-frame"
    ));
}

#[tokio::test]
async fn cancelling_read_preserves_prefetched_and_partially_decoded_frames() {
    let next = masked_text_frame("next-turn");
    let mut prefix = masked_text_frame("control");
    prefix.extend_from_slice(&next[..3]);
    let (mut peer, io) = duplex(64);
    let (mut reader, _) = split_websocket(io, &prefix, MAX_PROXY_REQUEST_BYTES, None).unwrap();
    assert_eq!(
        reader.read().await.unwrap(),
        WebSocketEvent::Text("control".into())
    );
    assert_pending(reader.read()).await;
    let moved_reader = tokio::spawn(async move { reader });
    let mut reader = moved_reader.await.unwrap();
    peer.write_all(&next[3..]).await.unwrap();
    assert_eq!(
        reader.read().await.unwrap(),
        WebSocketEvent::Text("next-turn".into())
    );
}

#[tokio::test]
async fn a_buffered_pong_does_not_hide_the_following_message() {
    let mut client = client_socket();
    client.frame_decoder.feed(b"\x8a\x00\x81\x02{}").unwrap();
    assert_eq!(
        client.read().await.unwrap(),
        WebSocketEvent::Text("{}".into())
    );

    let mut frames = vec![0x8a, 0x80, 1, 2, 3, 4];
    frames.extend(masked_text_frame("next"));
    let mut stream = Cursor::new(Vec::new());
    let mut downstream = WebSocketConnection::new_with_prefix(&mut stream, &frames).unwrap();
    assert_eq!(
        downstream.poll_text().unwrap(),
        WebSocketPoll::Text("next".into())
    );
}

#[test]
fn downstream_ping_is_returned_for_single_owner_pong_and_empty_close_is_preserved() {
    let mut ping = vec![0x89, 0x80 | 4, 1, 2, 3, 4];
    ping.extend([b'p' ^ 1, b'i' ^ 2, b'n' ^ 3, b'g' ^ 4]);
    let mut stream = Cursor::new(Vec::new());
    let mut connection = WebSocketConnection::new_with_prefix(&mut stream, &ping).unwrap();
    assert!(matches!(
        connection.poll_text().unwrap(),
        WebSocketPoll::Ping(payload) if payload == b"ping"
    ));
    connection.send_pong(b"ping").unwrap();
    drop(connection);
    assert_eq!(stream.into_inner(), [0x8a, 4, b'p', b'i', b'n', b'g']);

    let empty_close = vec![0x88, 0x80, 5, 6, 7, 8];
    let mut stream = Cursor::new(Vec::new());
    let mut connection = WebSocketConnection::new_with_prefix(&mut stream, &empty_close).unwrap();
    assert_eq!(
        connection.poll_text().unwrap(),
        WebSocketPoll::Closed { code: None }
    );
    assert_eq!(connection.peer_close_code(), Some(1005));
    drop(connection);
    assert_eq!(stream.into_inner(), [0x88, 0]);
}

#[test]
fn deflate_output_is_incrementally_limited_and_errors_are_classified() {
    let input = vec![b'x'; 4 * 1024 * 1024 + 1];
    let mut compressor = Compress::new(Compression::fast(), false);
    let mut compressed = Vec::with_capacity(8192);
    loop {
        let consumed = usize::try_from(compressor.total_in())
            .unwrap_or(input.len())
            .min(input.len());
        compressor
            .compress_vec(&input[consumed..], &mut compressed, FlushCompress::Sync)
            .unwrap();
        let consumed = usize::try_from(compressor.total_in())
            .unwrap_or(input.len())
            .min(input.len());
        if consumed == input.len() && compressed.ends_with(&[0, 0, 255, 255]) {
            compressed.truncate(compressed.len() - 4);
            break;
        }
        compressed.reserve(8192);
    }

    let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
    assert_eq!(
        deflate.decompress(&compressed, 4 * 1024 * 1024),
        Err(DecompressionError::TooLarge)
    );

    let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
    assert_eq!(
        deflate.decompress(&[0xff], 4 * 1024 * 1024),
        Err(DecompressionError::Invalid)
    );
}

fn finished_deflate(input: &[u8]) -> Vec<u8> {
    let mut compressor = Compress::new(Compression::fast(), false);
    let mut compressed = Vec::with_capacity(input.len() + 64);
    compressor
        .compress_vec(input, &mut compressed, FlushCompress::Finish)
        .unwrap();
    compressed
}

#[test]
fn deflate_stream_end_terminates_instead_of_spinning() {
    let message = br#"{"type":"response.completed"}"#;
    let finished = finished_deflate(message);

    let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
    assert_eq!(
        deflate.decompress(&finished, 4 * 1024 * 1024).as_deref(),
        Ok(&message[..])
    );
    // The decompressor restarts for the next message after BFINAL.
    let again = finished_deflate(message);
    assert_eq!(
        deflate.decompress(&again, 4 * 1024 * 1024).as_deref(),
        Ok(&message[..])
    );

    let mut trailing = finished_deflate(message);
    trailing.extend_from_slice(&[0, 0, 255, 255]);
    let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
    assert_eq!(
        deflate.decompress(&trailing, 4 * 1024 * 1024),
        Err(DecompressionError::Invalid)
    );

    // RFC 7692 section 7.2.3.4: "Hello" in a BFINAL block followed by
    // one 0x00 padding octet.
    let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
    assert_eq!(
        deflate
            .decompress(&[0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x00], 1024)
            .as_deref(),
        Ok(&b"Hello"[..])
    );

    let mut garbage = finished_deflate(message);
    garbage.extend_from_slice(b"leftover");
    let mut deflate = PerMessageDeflate::negotiated("permessage-deflate").unwrap();
    assert_eq!(
        deflate.decompress(&garbage, 4 * 1024 * 1024),
        Err(DecompressionError::Invalid)
    );
}

#[tokio::test]
async fn write_receipt_waits_for_flush_and_reports_flush_failure() {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, DuplexStream, ReadBuf};
    use tokio::sync::oneshot;

    struct GatedFlush {
        io: DuplexStream,
        release: oneshot::Receiver<io::Result<()>>,
    }

    impl AsyncRead for GatedFlush {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            output: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_read(cx, output)
        }
    }

    impl AsyncWrite for GatedFlush {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.io).poll_write(cx, bytes)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.release)
                .poll(cx)
                .map(|result| result.unwrap())
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }

    for fail in [false, true] {
        let (io, mut peer) = duplex(64);
        let (release, gate) = oneshot::channel();
        let (_, writer) = split_websocket(
            GatedFlush { io, release: gate },
            &[],
            MAX_PROXY_REQUEST_BYTES,
            None,
        )
        .unwrap();
        let mut write = std::pin::pin!(writer.send_text(b"committed"));
        assert_pending(write.as_mut()).await;
        let mut frame = [0; 11];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame, b"\x81\x09committed");
        assert_pending(write.as_mut()).await;
        release
            .send(if fail {
                Err(io::ErrorKind::ConnectionReset.into())
            } else {
                Ok(())
            })
            .unwrap();
        assert_eq!(write.await.is_err(), fail);
    }
}
