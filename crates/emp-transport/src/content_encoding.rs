use crate::{RequestBudget, RequestCapacityError};
use flate2::read::{GzDecoder, ZlibDecoder};
use std::fmt;
use std::io::{Cursor, Read};

const DECODE_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub enum ContentDecodeError {
    UnsupportedEncoding,
    InvalidCompressedBody,
    DecodedTooLarge { limit: usize },
    Capacity(RequestCapacityError),
}

impl ContentDecodeError {
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::Capacity(error) => error.http_status(),
            Self::DecodedTooLarge { .. } => 413,
            Self::UnsupportedEncoding | Self::InvalidCompressedBody => 400,
        }
    }
}

impl fmt::Display for ContentDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEncoding => formatter.write_str("unsupported Content-Encoding"),
            Self::InvalidCompressedBody => formatter.write_str("invalid compressed request body"),
            Self::DecodedTooLarge { limit } => write!(
                formatter,
                "decoded request body is too large (EMP limit: {limit} bytes)"
            ),
            Self::Capacity(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ContentDecodeError {}

fn capacity_error(mut error: RequestCapacityError) -> ContentDecodeError {
    error.decoded = true;
    ContentDecodeError::Capacity(error)
}

fn ensure_size(
    size: usize,
    max_length: usize,
    budget: &mut Option<&mut RequestBudget>,
) -> Result<(), ContentDecodeError> {
    if let Some(budget) = budget.as_deref_mut() {
        budget.ensure(size).map_err(capacity_error)
    } else if size > max_length {
        Err(ContentDecodeError::DecodedTooLarge { limit: max_length })
    } else {
        Ok(())
    }
}

fn read_bounded<R: Read>(
    mut reader: R,
    max_length: usize,
    budget: &mut Option<&mut RequestBudget>,
) -> Result<Vec<u8>, ContentDecodeError> {
    let mut decoded = Vec::new();
    let mut chunk = [0_u8; DECODE_CHUNK_BYTES];
    loop {
        let read_limit = if budget.is_some() {
            chunk.len()
        } else {
            chunk
                .len()
                .min(max_length.saturating_sub(decoded.len()).saturating_add(1))
        };
        let count = reader
            .read(&mut chunk[..read_limit])
            .map_err(|_| ContentDecodeError::InvalidCompressedBody)?;
        if count == 0 {
            break;
        }
        let projected = decoded
            .len()
            .checked_add(count)
            .ok_or(ContentDecodeError::DecodedTooLarge { limit: max_length })?;
        ensure_size(projected, max_length, budget)?;
        decoded.extend_from_slice(&chunk[..count]);
    }
    Ok(decoded)
}

fn decode_layer(
    value: Vec<u8>,
    encoding: &str,
    max_length: usize,
    budget: &mut Option<&mut RequestBudget>,
) -> Result<Vec<u8>, ContentDecodeError> {
    match encoding {
        "gzip" | "x-gzip" => read_bounded(GzDecoder::new(Cursor::new(value)), max_length, budget),
        "deflate" => read_bounded(ZlibDecoder::new(Cursor::new(value)), max_length, budget),
        "zstd" => {
            let decoder = zstd::stream::read::Decoder::new(Cursor::new(value))
                .map_err(|_| ContentDecodeError::InvalidCompressedBody)?;
            read_bounded(decoder, max_length, budget)
        }
        _ => Err(ContentDecodeError::UnsupportedEncoding),
    }
}

/// Encode one request body as a zstd frame using the Python binding's
/// default compression settings. The frame is self-contained and can be
/// decoded by any standard zstd decoder.
pub fn zstd_encode(value: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::bulk::compress(value, zstd::DEFAULT_COMPRESSION_LEVEL)
}

/// Decode HTTP content encodings in reverse application order under one
/// request budget. Every intermediate representation is bounded.
pub fn decode_content(
    value: Vec<u8>,
    content_encoding: &str,
    max_length: usize,
    mut budget: Option<&mut RequestBudget>,
) -> Result<Vec<u8>, ContentDecodeError> {
    let encodings = content_encoding
        .split(',')
        .map(str::trim)
        .filter(|encoding| !encoding.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|encoding| encoding != "identity")
        .collect::<Vec<_>>();
    let mut decoded = value;
    for encoding in encodings.iter().rev() {
        decoded = decode_layer(decoded, encoding, max_length, &mut budget)?;
    }
    ensure_size(decoded.len(), max_length, &mut budget)?;
    Ok(decoded)
}
