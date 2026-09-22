use emp_transport::{
    ContentDecodeError, MemoryStatus, RequestCapacityReason, RequestLimits, RequestLimitsConfig,
    TransportKind, decode_content,
};
use flate2::Compression;
use flate2::write::{GzEncoder, ZlibEncoder};
use std::io::Write;

fn gzip(value: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(value).unwrap();
    encoder.finish().unwrap()
}

fn deflate(value: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(value).unwrap();
    encoder.finish().unwrap()
}

fn small_limits(available: usize) -> std::sync::Arc<RequestLimits> {
    RequestLimits::new(
        RequestLimitsConfig {
            baseline: 64,
            maximum: 256,
            growth_quantum: 64,
            memory_headroom: 0,
        },
        move || Some(MemoryStatus::available(available)),
        || 0,
        "decode-test",
    )
    .unwrap()
}

#[test]
fn every_supported_encoding_and_composition_preserves_bytes() {
    let raw = b"preserve-history-".repeat(10);
    let gzip_value = gzip(&raw);
    let cases = vec![
        ("identity", raw.clone()),
        ("gzip", gzip_value.clone()),
        ("x-gzip", gzip_value.clone()),
        ("deflate", deflate(&raw)),
        ("zstd", zstd::stream::encode_all(raw.as_slice(), 0).unwrap()),
        (
            "gzip, zstd",
            zstd::stream::encode_all(gzip_value.as_slice(), 0).unwrap(),
        ),
    ];
    for (encoding, encoded) in cases {
        assert_eq!(
            decode_content(encoded, encoding, 1024, None).unwrap(),
            raw,
            "{encoding}"
        );
    }
}

#[test]
fn expansion_uses_the_same_request_budget() {
    let raw = b"preserve-history-".repeat(10);
    let limits = small_limits(16_384);
    let mut budget = limits.request(TransportKind::Http);
    assert_eq!(
        decode_content(gzip(&raw), "gzip", 64, Some(&mut budget)).unwrap(),
        raw
    );
    assert_eq!(budget.limit(), 192);
    assert_eq!(budget.reserved(), 192 * 8);
}

#[test]
fn decoded_hard_limit_and_memory_pressure_remain_distinct() {
    let compressed = gzip(&[b'x'; 300]);
    let limits = small_limits(16_384);
    let mut budget = limits.request(TransportKind::Http);
    match decode_content(compressed, "gzip", 64, Some(&mut budget)) {
        Err(ContentDecodeError::Capacity(error)) => {
            assert_eq!(error.reason, RequestCapacityReason::HardLimit);
            assert!(error.decoded);
            assert_eq!(error.http_status(), 413);
        }
        result => panic!("expected decoded hard limit, got {result:?}"),
    }

    let memory_limits = small_limits(100);
    let mut memory_budget = memory_limits.request(TransportKind::Http);
    match decode_content(gzip(&[b'x'; 100]), "gzip", 64, Some(&mut memory_budget)) {
        Err(ContentDecodeError::Capacity(error)) => {
            assert_eq!(error.reason, RequestCapacityReason::MemoryLimit);
            assert!(error.decoded);
            assert_eq!(error.http_status(), 503);
        }
        result => panic!("expected decoded memory limit, got {result:?}"),
    }
}

#[test]
fn fixed_limit_invalid_streams_and_unknown_encodings_fail_closed() {
    let compressed = gzip(&[b'x'; 65]);
    assert!(matches!(
        decode_content(compressed, "gzip", 64, None),
        Err(ContentDecodeError::DecodedTooLarge { limit: 64 })
    ));
    assert!(matches!(
        decode_content(b"not compressed".to_vec(), "gzip", 64, None),
        Err(ContentDecodeError::InvalidCompressedBody)
    ));
    assert!(matches!(
        decode_content(b"secret".to_vec(), "br", 64, None),
        Err(ContentDecodeError::UnsupportedEncoding)
    ));

    let raw = b"must not accept a truncated compressed body".repeat(10);
    let mut compressed = vec![
        ("gzip", gzip(&raw)),
        ("deflate", deflate(&raw)),
        ("zstd", zstd::stream::encode_all(raw.as_slice(), 0).unwrap()),
    ];
    for (encoding, body) in &mut compressed {
        body.truncate(body.len() / 2);
        assert!(matches!(
            decode_content(std::mem::take(body), encoding, 1024, None),
            Err(ContentDecodeError::InvalidCompressedBody)
        ));
    }
}
