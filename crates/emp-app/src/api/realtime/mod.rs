//! Codex Voice HTTP call creation and native sideband service.

use crate::http::response::{json_error_response, status_text};
use serde_json::Value;
use std::collections::BTreeMap;

mod call;
pub(crate) mod multipart;
pub(crate) mod sideband;

pub(crate) use call::serve_realtime_call;

pub(crate) const MAX_REALTIME_REQUEST_BYTES: usize = 256 * 1024;
pub(crate) const MAX_REALTIME_PART_BYTES: usize = 128 * 1024;
pub(crate) const MAX_REALTIME_PART_HEADER_BYTES: usize = 4096;
const MAX_REALTIME_RESPONSE_BYTES: usize = 256 * 1024;
const REALTIME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CALL_ID_PREFIX: &str = "rtc_";

pub(crate) fn valid_call_id(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix(CALL_ID_PREFIX) {
        return !rest.is_empty()
            && rest.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-')
            });
    }
    let segments = value.split('-').collect::<Vec<_>>();
    segments.len() == 5
        && [8, 4, 4, 4, 12]
            .into_iter()
            .zip(segments)
            .all(|(length, segment)| {
                segment.len() == length && segment.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RealtimeCall {
    pub(crate) sdp: String,
    pub(crate) session: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealtimeResponse {
    pub(crate) status: u16,
    pub(crate) content_type: String,
    pub(crate) body: Vec<u8>,
    pub(crate) location: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RealtimeError {
    pub(crate) status: u16,
    pub(crate) code: &'static str,
    pub(crate) message: String,
    pub(crate) close: bool,
}

impl RealtimeError {
    pub(crate) fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            close: false,
        }
    }

    pub(crate) fn closing(mut self) -> Self {
        self.close = true;
        self
    }

    pub(crate) fn wire_response(&self) -> Vec<u8> {
        json_error_response(
            self.status,
            status_text(self.status),
            &self.message,
            Some(self.code),
            &[],
        )
    }
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}
