use serde_json::{Map, Value};
use std::fmt;

mod admission;
mod content_encoding;
mod failure;
mod http_client;
mod http_policy;
mod system_memory;
mod system_proxy;
mod websocket;
pub use admission::{
    MAX_EXPANDED_REQUEST_BYTES, MAX_PROXY_REQUEST_BYTES, MEMORY_RESERVATION_FACTOR,
    MIN_MEMORY_HEADROOM_BYTES, MemoryStatus, REQUEST_GROWTH_QUANTUM, RequestBudget,
    RequestCapacityError, RequestCapacityReason, RequestLimitNotice, RequestLimits,
    RequestLimitsConfig, RequestLimitsError, RequestLimitsSnapshot, TransportKind,
};
pub use content_encoding::{ContentDecodeError, decode_content, zstd_encode};
pub use failure::{
    FailureClass, FailurePhase, HttpFailureInput, NetworkFailureKind, UpstreamFailure,
    http_failure, network_failure, normalize_error_class, public_failure_message,
    status_error_class,
};
pub use http_client::{
    HttpClient, HttpClientConfig, HttpResponse, HttpTransportError, HttpTransportErrorKind,
};
pub use http_policy::{
    ConnectionPoolPolicy, HttpClientPolicy, HttpClientPolicyError, HttpMethod, ProxyEnvironment,
    ProxyOrigin, ProxyPolicy, ProxySnapshot, ProxySource, RequestPlan, TimeoutPolicy,
};
pub use system_memory::system_memory_status;
#[doc(hidden)]
pub use websocket::websocket_codec;
pub use websocket::{
    AsyncClientWebSocket, ClientWebSocketError, DEFAULT_WEBSOCKET_MESSAGE_BYTES, LocalWebSocket,
    MAX_RESPONSES_WEBSOCKET_BYTES, WebSocketError, WebSocketEvent, WebSocketReader,
    WebSocketTraffic, WebSocketWriter, split_websocket, websocket_accept,
};

pub const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum SseFrame {
    Json(Map<String, Value>),
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorReason {
    SseEventTooLarge,
    SseInvalidJson,
    SseNonObject,
}

impl TransportErrorReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SseEventTooLarge => "sse_event_too_large",
            Self::SseInvalidJson => "sse_invalid_json",
            Self::SseNonObject => "sse_non_object",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    reason: TransportErrorReason,
    message: &'static str,
}

impl TransportError {
    fn new(reason: TransportErrorReason, message: &'static str) -> Self {
        Self { reason, message }
    }

    pub const fn reason(&self) -> TransportErrorReason {
        self.reason
    }

    pub const fn failure_reason(&self) -> &'static str {
        self.reason.as_str()
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for TransportError {}

/// Parse SSE data JSON across network chunks, ignoring unknown fields and comments;
/// `[DONE]` ends only its event because compatible gateways may send a terminal event afterward.
#[derive(Debug, Clone)]
pub struct SseJsonParser {
    limit: usize,
    pending: Vec<u8>,
    skip_line: bool,
    discard_event: bool,
    data_lines: Vec<Vec<u8>>,
    data_bytes: usize,
}

impl Default for SseJsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseJsonParser {
    pub fn new() -> Self {
        Self::with_limit(MAX_SSE_EVENT_BYTES).expect("default SSE limit is positive")
    }

    pub fn with_limit(limit: usize) -> Result<Self, TransportError> {
        if limit == 0 {
            return Err(Self::too_large());
        }
        Ok(Self {
            limit,
            pending: Vec::new(),
            skip_line: false,
            discard_event: false,
            data_lines: Vec::new(),
            data_bytes: 0,
        })
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Map<String, Value>>, TransportError> {
        Ok(self
            .push_frames(chunk)?
            .into_iter()
            .filter_map(|frame| match frame {
                SseFrame::Json(value) => Some(value),
                SseFrame::Done => None,
            })
            .collect())
    }

    pub fn push_frames(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, TransportError> {
        self.push_frame_results(chunk).into_iter().collect()
    }

    pub fn push_frame_results(&mut self, chunk: &[u8]) -> Vec<Result<SseFrame, TransportError>> {
        let mut output = Vec::new();
        for fragment in chunk.split_inclusive(|byte| *byte == b'\n') {
            let complete = fragment.ends_with(b"\n");
            let line = fragment.strip_suffix(b"\n").unwrap_or(fragment);
            if self.skip_line {
                self.skip_line = !complete;
                continue;
            }
            if self.pending.len().saturating_add(line.len()) > self.limit {
                self.pending.clear();
                self.skip_line = !complete;
                if !self.discard_event {
                    self.discard_event = true;
                    self.data_lines.clear();
                    self.data_bytes = 0;
                    output.push(Err(Self::too_large()));
                }
                continue;
            }
            if !complete {
                self.pending.extend_from_slice(line);
                break;
            }
            if self.pending.is_empty() {
                self.consume_line(line, &mut output);
            } else {
                let mut pending = std::mem::take(&mut self.pending);
                pending.extend_from_slice(line);
                self.consume_line(&pending, &mut output);
                pending.clear();
                self.pending = pending;
            }
        }
        output
    }

    pub fn finish(self) -> Result<Vec<Map<String, Value>>, TransportError> {
        Ok(self
            .finish_frames()?
            .into_iter()
            .filter_map(|frame| match frame {
                SseFrame::Json(value) => Some(value),
                SseFrame::Done => None,
            })
            .collect())
    }

    pub fn finish_frames(mut self) -> Result<Vec<SseFrame>, TransportError> {
        let mut output = Vec::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.consume_line(&line, &mut output);
        }
        if !self.discard_event {
            output.extend(self.finish_event().transpose());
        }
        output.into_iter().collect()
    }

    fn consume_line(&mut self, raw: &[u8], output: &mut Vec<Result<SseFrame, TransportError>>) {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() {
            self.discard_event = false;
            output.extend(self.finish_event().transpose());
        } else if !self.discard_event
            && let Some(value) = line.strip_prefix(b"data:")
            && let Err(error) = self.append_data(trim_ascii_start(value))
        {
            self.discard_event = true;
            self.data_lines.clear();
            self.data_bytes = 0;
            output.push(Err(error));
        }
    }

    fn append_data(&mut self, value: &[u8]) -> Result<(), TransportError> {
        let separator = usize::from(!self.data_lines.is_empty());
        let projected = self
            .data_bytes
            .checked_add(value.len())
            .and_then(|size| size.checked_add(separator))
            .ok_or_else(Self::too_large)?;
        if projected > self.limit {
            return Err(Self::too_large());
        }
        self.data_lines.push(value.to_vec());
        self.data_bytes = projected;
        Ok(())
    }

    fn finish_event(&mut self) -> Result<Option<SseFrame>, TransportError> {
        if self.data_lines.is_empty() {
            return Ok(None);
        }
        let mut raw = Vec::with_capacity(self.data_bytes);
        for (index, line) in self.data_lines.drain(..).enumerate() {
            if index != 0 {
                raw.push(b'\n');
            }
            raw.extend(line);
        }
        self.data_bytes = 0;
        if raw == b"[DONE]" {
            return Ok(Some(SseFrame::Done));
        }
        let value: Value = serde_json::from_slice(&raw).map_err(|_| {
            Self::error(
                TransportErrorReason::SseInvalidJson,
                "upstream SSE event is not valid JSON",
            )
        })?;
        value
            .as_object()
            .cloned()
            .map(SseFrame::Json)
            .map(Some)
            .ok_or_else(|| {
                Self::error(
                    TransportErrorReason::SseNonObject,
                    "upstream SSE event must be a JSON object",
                )
            })
    }

    fn error(reason: TransportErrorReason, message: &'static str) -> TransportError {
        TransportError::new(reason, message)
    }

    fn too_large() -> TransportError {
        Self::error(
            TransportErrorReason::SseEventTooLarge,
            "upstream SSE event is too large",
        )
    }
}

fn trim_ascii_start(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    value
}

pub fn sse_json_events<I, B>(chunks: I) -> Result<Vec<Map<String, Value>>, TransportError>
where
    I: IntoIterator<Item = B>,
    B: AsRef<[u8]>,
{
    let mut parser = SseJsonParser::new();
    let mut output = Vec::new();
    for chunk in chunks {
        output.extend(parser.push(chunk.as_ref())?);
    }
    output.extend(parser.finish()?);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_frame_cannot_grow_buffers_while_waiting_for_its_blank_line() {
        let mut parser = SseJsonParser::with_limit(32).unwrap();
        assert!(
            parser
                .push_frame_results(b"data: {\"abandoned\":\n")
                .is_empty()
        );
        assert!(parser.push_frame_results(b"data: ").is_empty());
        let capacity = parser.pending.capacity();
        assert_eq!(
            parser
                .push(b"abcdefghijklmnopqrstuvwxyz0123456789")
                .unwrap_err()
                .reason(),
            TransportErrorReason::SseEventTooLarge
        );
        for _ in 0..16 {
            assert!(parser.push_frame_results(&[b'x'; 4096]).is_empty());
            assert!(parser.pending.is_empty());
            assert_eq!(parser.pending.capacity(), capacity);
            assert!(parser.data_lines.is_empty());
            assert_eq!(parser.data_bytes, 0);
        }
        let events = parser
            .push(b"\r\ndata: {\"ignored\":true}\r\n\r\ndata: {\"next\":true}\r\n\r\n")
            .unwrap();
        assert_eq!(
            events,
            vec![
                serde_json::json!({"next": true})
                    .as_object()
                    .unwrap()
                    .clone()
            ]
        );
    }
}
