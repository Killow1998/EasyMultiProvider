//! RFC 7692 negotiation and bounded compression, independent of socket I/O.
use super::ClientWebSocketError;
use crate::MAX_PROXY_REQUEST_BYTES;
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

pub(super) struct PerMessageDeflate {
    compressor: Compress,
    decompressor: Decompress,
    client_no_context_takeover: bool,
    server_no_context_takeover: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DecompressionError {
    Invalid,
    TooLarge,
}

impl PerMessageDeflate {
    pub(super) fn negotiated(value: &str) -> Result<Self, ClientWebSocketError> {
        let mut parts = value.split(';').map(str::trim);
        if parts.next() != Some("permessage-deflate") {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket extension is unsupported",
            ));
        }
        let mut client_bits = 15_u8;
        let mut server_bits = 15_u8;
        let mut client_no_context_takeover = false;
        let mut server_no_context_takeover = false;
        for parameter in parts {
            if parameter == "client_no_context_takeover" {
                client_no_context_takeover = true;
            } else if parameter == "server_no_context_takeover" {
                server_no_context_takeover = true;
            } else if let Some(value) = parameter.strip_prefix("client_max_window_bits=") {
                client_bits = value.parse().map_err(|_| {
                    ClientWebSocketError::new(502, "native websocket extension is invalid")
                })?;
            } else if let Some(value) = parameter.strip_prefix("server_max_window_bits=") {
                server_bits = value.parse().map_err(|_| {
                    ClientWebSocketError::new(502, "native websocket extension is invalid")
                })?;
            } else if !parameter.is_empty() {
                return Err(ClientWebSocketError::new(
                    502,
                    "native websocket extension is unsupported",
                ));
            }
        }
        if !(9..=15).contains(&client_bits) || !(9..=15).contains(&server_bits) {
            return Err(ClientWebSocketError::new(
                502,
                "native websocket extension is invalid",
            ));
        }
        Ok(Self {
            compressor: Compress::new_with_window_bits(Compression::fast(), false, client_bits),
            decompressor: Decompress::new_with_window_bits(false, server_bits),
            client_no_context_takeover,
            server_no_context_takeover,
        })
    }

    pub(super) fn compress(&mut self, payload: &[u8]) -> Result<Vec<u8>, ClientWebSocketError> {
        let before_in = self.compressor.total_in();
        let mut output = Vec::with_capacity(payload.len().saturating_add(64));
        loop {
            output.reserve(8192);
            let consumed = usize::try_from(self.compressor.total_in() - before_in)
                .unwrap_or(payload.len())
                .min(payload.len());
            self.compressor
                .compress_vec(&payload[consumed..], &mut output, FlushCompress::Sync)
                .map_err(|_| {
                    ClientWebSocketError::new(502, "native websocket compression failed")
                })?;
            let consumed =
                usize::try_from(self.compressor.total_in() - before_in).unwrap_or(payload.len());
            if consumed >= payload.len() && output.ends_with(&[0, 0, 255, 255]) {
                output.truncate(output.len() - 4);
                break;
            }
            if output.len() > MAX_PROXY_REQUEST_BYTES {
                return Err(ClientWebSocketError::new(
                    413,
                    "native websocket request is too large",
                ));
            }
        }
        if self.client_no_context_takeover {
            self.compressor.reset();
        }
        Ok(output)
    }

    pub(super) fn decompress(
        &mut self,
        payload: &[u8],
        max_message_bytes: usize,
    ) -> Result<Vec<u8>, DecompressionError> {
        let mut encoded = Vec::with_capacity(payload.len() + 4);
        encoded.extend_from_slice(payload);
        encoded.extend_from_slice(&[0, 0, 255, 255]);
        let before_in = self.decompressor.total_in();
        let mut output = Vec::with_capacity(max_message_bytes.saturating_add(1).min(8192));
        loop {
            let remaining = max_message_bytes
                .saturating_add(1)
                .saturating_sub(output.len());
            let wanted = remaining.min(8192);
            let spare = output.capacity().saturating_sub(output.len());
            if spare < wanted {
                output
                    .try_reserve_exact(wanted - spare)
                    .map_err(|_| DecompressionError::TooLarge)?;
            }
            let total_in_before = self.decompressor.total_in();
            let output_before = output.len();
            let consumed = usize::try_from(total_in_before - before_in)
                .unwrap_or(encoded.len())
                .min(encoded.len());
            let status = self
                .decompressor
                .decompress_vec(&encoded[consumed..], &mut output, FlushDecompress::Sync)
                .map_err(|_| DecompressionError::Invalid)?;
            if output.len() > max_message_bytes {
                return Err(DecompressionError::TooLarge);
            }
            let consumed =
                usize::try_from(self.decompressor.total_in() - before_in).unwrap_or(encoded.len());
            if status == Status::StreamEnd {
                // The peer finished the DEFLATE stream with a BFINAL block
                // (RFC 7692 section 7.2.3.4). Only the synthetic empty-block
                // tail may remain, optionally after the single 0x00 octet the
                // RFC's own example sends (an empty non-final stored-block
                // header); any other payload bytes after the end of the
                // stream are malformed. The next message starts a new stream.
                self.decompressor.reset(false);
                if !matches!(payload.get(consumed..), Some([] | [0x00])) {
                    return Err(DecompressionError::Invalid);
                }
                return Ok(output);
            }
            if consumed >= encoded.len() {
                break;
            }
            if self.decompressor.total_in() == total_in_before && output.len() == output_before {
                return Err(DecompressionError::Invalid);
            }
        }
        if self.server_no_context_takeover {
            self.decompressor.reset(false);
        }
        Ok(output)
    }
}
