//! Response reads own decoded limits, deadlines and bounded connection cleanup.
use super::{HttpTransportError, HttpTransportErrorKind};
use crate::http_policy::{RequestPlan, TimeoutPolicy};
use reqwest::{StatusCode, header::HeaderMap};
use std::{fmt, time::Instant};
const CLEANUP_BYTE_LIMIT: usize = 64 * 1024;

fn map_read_error(error: reqwest::Error) -> HttpTransportError {
    if error.is_timeout() {
        HttpTransportError::new(HttpTransportErrorKind::ReadTimeout)
    } else {
        HttpTransportError::new(HttpTransportErrorKind::Network)
    }
}

pub struct HttpResponse {
    response: Option<reqwest::Response>,
    status: StatusCode,
    headers: HeaderMap,
    timeout_policy: TimeoutPolicy,
    non_stream_deadline: Instant,
    stream: bool,
    output_emitted: bool,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status.as_u16())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("stream", &self.stream)
            .field("output_emitted", &self.output_emitted)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    pub(super) fn new(response: reqwest::Response, plan: RequestPlan, started_at: Instant) -> Self {
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            response: Some(response),
            timeout_policy: plan.timeout_policy,
            non_stream_deadline: plan.timeout_policy.non_stream_deadline(started_at),
            stream: plan.stream,
            output_emitted: false,
        }
    }

    pub fn status(&self) -> u16 {
        self.status.as_u16()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Iterate string-valued response headers without copying them.
    ///
    /// Duplicate names are preserved; non-visible-ASCII values are omitted. This
    /// accessor never logs header values.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.headers.iter().filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str(), value))
        })
    }

    pub fn is_stream(&self) -> bool {
        self.stream
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpTransportError> {
        let timeout = if self.output_emitted {
            self.timeout_policy.stream_idle
        } else {
            self.timeout_policy.first_event
        };
        let response = self
            .response
            .as_mut()
            .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::InvalidRequest))?;
        let chunk = tokio::time::timeout(timeout, response.chunk())
            .await
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?
            .map_err(map_read_error)?;
        if chunk.as_ref().is_some_and(|chunk| !chunk.is_empty()) {
            self.output_emitted = true;
        }
        Ok(chunk.map(|chunk| chunk.to_vec()))
    }

    pub async fn read_all(self) -> Result<Vec<u8>, HttpTransportError> {
        self.read_limited(usize::MAX).await
    }

    pub async fn read_limited(self, limit: usize) -> Result<Vec<u8>, HttpTransportError> {
        self.read_body(limit, false).await
    }

    /// Read at most `limit` bytes and discard the rest of this response. Unlike
    /// read_limited, a larger declared or streamed body is not an error.
    pub async fn read_prefix(self, limit: usize) -> Result<Vec<u8>, HttpTransportError> {
        self.read_body(limit, true).await
    }

    async fn read_body(
        mut self,
        limit: usize,
        prefix: bool,
    ) -> Result<Vec<u8>, HttpTransportError> {
        if prefix && limit == 0 {
            return Ok(Vec::new());
        }
        if !prefix
            && self
                .header("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > limit)
        {
            return Err(HttpTransportError::new(
                HttpTransportErrorKind::ResponseTooLarge,
            ));
        }
        let remaining = self
            .non_stream_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?;
        let mut response = self
            .response
            .take()
            .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::InvalidRequest))?;
        let deadline = Instant::now() + remaining;
        let mut result = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?;
            let chunk = tokio::time::timeout(remaining, response.chunk())
                .await
                .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?
                .map_err(map_read_error)?;
            let Some(chunk) = chunk else {
                return Ok(result);
            };
            if prefix {
                let count = chunk.len().min(limit - result.len());
                result.extend_from_slice(&chunk[..count]);
                if result.len() == limit {
                    return Ok(result);
                }
                continue;
            }
            let next_length = result
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::ResponseTooLarge))?;
            if next_length > limit {
                return Err(HttpTransportError::new(
                    HttpTransportErrorKind::ResponseTooLarge,
                ));
            }
            result.extend_from_slice(&chunk);
        }
    }

    pub async fn finish(mut self) {
        let Some(mut response) = self.response.take() else {
            return;
        };
        let deadline = Instant::now() + self.timeout_policy.cleanup;
        let mut remaining = CLEANUP_BYTE_LIMIT;
        while remaining > 0 {
            let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let chunk = match tokio::time::timeout(timeout, response.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => return,
                Ok(Err(_)) | Err(_) => break,
            };
            if chunk.len() > remaining {
                break;
            }
            remaining -= chunk.len();
        }
    }
}
