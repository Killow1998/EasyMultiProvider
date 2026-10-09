use super::compression::{DecompressionError, PerMessageDeflate};
use super::network::ReadWrite;
use super::*;
use crate::websocket_pump::WebSocketPumpConfig;
use crate::{
    MAX_PROXY_REQUEST_BYTES,
    websocket_pump::{FrameDecoder, WebSocketPoll},
};
use flate2::{Compress, Compression, FlushCompress};
use std::io::Cursor;
use std::time::Duration;

impl ReadWrite for Cursor<Vec<u8>> {
    fn set_read_timeout(&self, _timeout: Option<Duration>) -> std::io::Result<()> {
        Ok(())
    }
}

fn client_socket() -> ClientWebSocket {
    ClientWebSocket {
        stream: Box::new(Cursor::new(Vec::new())),
        closed: false,
        peer_close_code: None,
        response_headers: Default::default(),
        compression: None,
        max_message_bytes: MAX_PROXY_REQUEST_BYTES,
        frame_decoder: FrameDecoder::new(MAX_PROXY_REQUEST_BYTES),
        local_control: false,
    }
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
    assert_eq!(WebSocketPumpConfig::default().max_message_bytes, FOUR_MIB);
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

#[test]
fn temporary_reader_preserves_prefetched_and_partially_decoded_frames() {
    let next = masked_text_frame("next-turn");
    let mut prefix = masked_text_frame("control");
    prefix.extend_from_slice(&next[..3]);
    let mut original = Cursor::new(Vec::new());
    let mut input = Cursor::new(next[3..].to_vec());
    let mut connection = WebSocketConnection::new_with_prefix(&mut original, &prefix).unwrap();
    let mut reader = connection.take_reader(&mut input).unwrap();
    assert!(
        connection.poll_text().is_err(),
        "two readers cannot own input"
    );
    assert_eq!(
        reader.poll_text().unwrap(),
        WebSocketPoll::Text("control".into())
    );
    connection.reclaim_reader(reader).unwrap();
    connection.feed_read_bytes(&next[3..]).unwrap();
    assert_eq!(
        connection.poll_text().unwrap(),
        WebSocketPoll::Text("next-turn".into())
    );
}

#[test]
fn a_buffered_pong_does_not_hide_the_following_message_from_readiness_consumers() {
    let mut client = client_socket();
    client.frame_decoder.feed(b"\x8a\x00\x81\x02{}").unwrap();
    assert_eq!(
        client.poll_receive_text().unwrap(),
        WebSocketPoll::Text("{}".into())
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
