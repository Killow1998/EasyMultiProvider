use super::{error::EntryError, raw_head};
use crate::api::realtime::{
    RealtimeCall, RealtimeError,
    multipart::{RealtimeRead, validate_headers},
};
use crate::app::ServerState;
use crate::http::request::{REQUEST_READ_TIMEOUT, Request, request_body_timeout};
use crate::server::run_blocking;
use axum::extract::{FromRequest, Request as AxumRequest};
use emp_transport::ContentDecodeError;
use emp_transport::MEMORY_RESERVATION_FACTOR;
use emp_transport::MIN_MEMORY_HEADROOM_BYTES;
use emp_transport::REQUEST_GROWTH_QUANTUM;
use emp_transport::RequestBudget;
use emp_transport::RequestCapacityError;
use emp_transport::RequestCapacityReason;
use emp_transport::RequestLimits;
use emp_transport::RequestLimitsConfig;
use emp_transport::TransportKind;
use emp_transport::decode_content;
use hyper::body::Body as _;
use serde_json::Value;
use std::{future::poll_fn, pin::Pin, sync::Arc};
use tokio::time::{Instant, timeout_at};

pub(crate) enum BodyError {
    Invalid(String),
    Timeout,
    Capacity(RequestCapacityError),
    Decode(ContentDecodeError),
}

fn body_size_error(limit: usize, decoded: bool) -> BodyError {
    BodyError::Capacity(RequestCapacityError {
        limit,
        decoded,
        reason: emp_transport::RequestCapacityReason::HardLimit,
        available_bytes: None,
        required_memory_bytes: 0,
        memory_total_bytes: None,
        memory_used_bytes: None,
        memory_used_percent: None,
    })
}

fn allocation_capacity_error(limit: usize) -> BodyError {
    let memory = emp_transport::system_memory_status();
    let reserved_limit = limit
        .div_ceil(REQUEST_GROWTH_QUANTUM)
        .saturating_mul(REQUEST_GROWTH_QUANTUM);
    BodyError::Capacity(RequestCapacityError {
        limit: reserved_limit,
        decoded: false,
        reason: RequestCapacityReason::MemoryLimit,
        available_bytes: memory.map(|memory| memory.available),
        required_memory_bytes: reserved_limit
            .saturating_mul(MEMORY_RESERVATION_FACTOR)
            .saturating_add(MIN_MEMORY_HEADROOM_BYTES),
        memory_total_bytes: memory.and_then(|memory| memory.total),
        memory_used_bytes: memory.and_then(|memory| memory.used),
        memory_used_percent: memory
            .and_then(|memory| memory.used_percent)
            .filter(|value| value.is_finite()),
    })
}

struct JsonRead {
    length: usize,
    limit: usize,
    budget: Option<RequestBudget>,
}

impl JsonRead {
    fn ensure(&mut self, length: usize) -> Result<(), BodyError> {
        if let Some(budget) = &mut self.budget {
            budget.ensure(length).map_err(BodyError::Capacity)?;
        } else if length > self.limit {
            return Err(body_size_error(self.limit, false));
        }
        Ok(())
    }
}

impl BodyError {
    fn incomplete() -> Self {
        Self::Invalid("request body is incomplete".to_owned())
    }
}

fn prepare_json(request: Request<'_>, limits: &Arc<RequestLimits>) -> Result<JsonRead, BodyError> {
    let content_type = request
        .header("Content-Type")
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or_default();
    if !content_type.eq_ignore_ascii_case("application/json") {
        return Err(BodyError::Invalid(
            "Content-Type must be application/json".to_owned(),
        ));
    }
    request.validate_body_framing()?;
    let raw_length = request.header("Content-Length").unwrap_or("0");
    if raw_length.starts_with('-') {
        if raw_length.parse::<i128>().is_ok_and(|value| value < 0) {
            return Err(BodyError::Invalid(
                "Content-Length cannot be negative".to_owned(),
            ));
        }
        return Err(BodyError::Invalid("invalid Content-Length".to_owned()));
    }
    let length = raw_length
        .parse::<u128>()
        .map_err(|_| BodyError::Invalid("invalid Content-Length".to_owned()))?;
    let management = request.raw_path().starts_with("/api/");
    let limit = if management {
        5 * 1024 * 1024
    } else {
        RequestLimitsConfig::default().maximum
    };
    let length = usize::try_from(length).map_err(|_| body_size_error(limit, false))?;
    let mut prepared = JsonRead {
        length,
        limit,
        budget: (!management).then(|| limits.request(TransportKind::Http)),
    };
    prepared.ensure(length)?;
    Ok(prepared)
}

fn decode_json(
    body: Vec<u8>,
    content_encoding: &str,
    mut input: JsonRead,
) -> Result<Value, BodyError> {
    let body = decode_content(body, content_encoding, input.limit, input.budget.as_mut()).map_err(
        |error| match error {
            ContentDecodeError::DecodedTooLarge { limit } => body_size_error(limit, true),
            error => BodyError::Decode(error),
        },
    )?;
    let decoded_size = body.len();
    let value: Value = serde_json::from_slice(&body)
        .map_err(|error| BodyError::Invalid(format!("request body must be valid JSON: {error}")))?;
    drop(body);
    crate::util::release_large_temporary_pages(decoded_size);
    if !value.is_object() {
        return Err(BodyError::Invalid(
            "request body must be a JSON object".to_owned(),
        ));
    }
    Ok(value)
}

pub(crate) struct JsonBody<T = Value>(pub(crate) T);

#[cfg(test)]
impl JsonBody {
    pub(crate) fn into_typed<T: serde::de::DeserializeOwned>(self) -> Result<T, EntryError> {
        serde_json::from_value(self.0).map_err(|error| {
            BodyError::Invalid(format!("request body must be valid JSON: {error}")).into()
        })
    }
}

impl FromRequest<Arc<ServerState>> for JsonBody {
    type Rejection = EntryError;

    async fn from_request(
        request: AxumRequest,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        Self::read(request, &state.backend.transport.request_limits).await
    }
}

impl JsonBody {
    pub(super) async fn read(
        request: AxumRequest,
        limits: &Arc<RequestLimits>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(request.extensions())?;
        let mut config = prepare_json(raw.request(), limits)?;
        let encoding = raw
            .request()
            .header("Content-Encoding")
            .unwrap_or_default()
            .to_owned();
        let raw = raw.clone();
        let mut body = Vec::new();
        read_json_body(
            request.into_body(),
            &raw,
            &mut config,
            &mut body,
            Vec::try_reserve_exact,
        )
        .await?;
        let value = run_blocking(move || decode_json(body, &encoding, config))
            .await
            .map_err(|_| EntryError::internal())??;
        Ok(Self(value))
    }
}

async fn read_json_body(
    body: axum::body::Body,
    raw: &crate::server::head::RawRequestHead,
    config: &mut JsonRead,
    bytes: &mut Vec<u8>,
    reserve: impl FnOnce(&mut Vec<u8>, usize) -> Result<(), std::collections::TryReserveError>,
) -> Result<(), BodyError> {
    let length = config.length;
    reserve(bytes, length).map_err(|_| allocation_capacity_error(length))?;
    raw.allow_body();
    read_body(body, length, bytes, |bytes, chunk| {
        let projected = bytes.len() + chunk.len();
        if projected > length {
            config.ensure(projected)?;
            return Err(BodyError::incomplete());
        }
        bytes.extend_from_slice(chunk);
        Ok(())
    })
    .await
}

pub(crate) struct RealtimeBody(pub(crate) RealtimeCall);

impl FromRequest<Arc<ServerState>> for RealtimeBody {
    type Rejection = EntryError;

    async fn from_request(
        request: AxumRequest,
        _state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(request.extensions())?;
        let head = raw.request();
        validate_headers(head)?;
        let input = RealtimeRead::prepare(&head.incoming())?;
        raw.allow_body();
        let mut bytes = Vec::new();
        read_body(
            request.into_body(),
            input.length,
            &mut bytes,
            |bytes, chunk| -> Result<(), RealtimeError> {
                input.reserve(bytes, bytes.len() + chunk.len())?;
                bytes.extend_from_slice(chunk);
                Ok(())
            },
        )
        .await?;
        let call = run_blocking(move || input.parse(&bytes))
            .await
            .map_err(|_| EntryError::internal())??;
        Ok(Self(call))
    }
}

impl From<BodyError> for RealtimeError {
    fn from(error: BodyError) -> Self {
        match error {
            BodyError::Timeout => RealtimeRead::timeout(),
            _ => RealtimeRead::incomplete(),
        }
    }
}

async fn read_body<E: From<BodyError>>(
    mut body: axum::body::Body,
    length: usize,
    bytes: &mut Vec<u8>,
    mut append: impl FnMut(&mut Vec<u8>, &[u8]) -> Result<(), E>,
) -> Result<(), E> {
    let deadline = Instant::now() + request_body_timeout(length);
    while bytes.len() < length || !body.is_end_stream() {
        let until = deadline.min(Instant::now() + REQUEST_READ_TIMEOUT);
        let frame = timeout_at(until, poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)))
            .await
            .map_err(|_| BodyError::Timeout)?;
        if Instant::now() >= deadline {
            return Err(BodyError::Timeout.into());
        }
        let Some(frame) = frame else {
            break;
        };
        let frame = frame.map_err(|_| BodyError::incomplete())?;
        if let Ok(chunk) = frame.into_data() {
            append(bytes, &chunk)?;
        }
    }
    if bytes.len() != length {
        return Err(BodyError::incomplete().into());
    }
    Ok(())
}

#[cfg(test)]
mod body_capacity_tests {
    use super::*;
    use crate::server::{
        head::{GuardedHead, RawRequestHead},
        http::tests::json_body,
    };
    use emp_test_support::test_timeout;

    async fn json_head(length: usize) -> RawRequestHead {
        let head = format!(
            "POST /api/config HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n\r\n"
        );
        let deadline = Instant::now() + test_timeout(REQUEST_READ_TIMEOUT).duration();
        GuardedHead::read(
            &mut head.as_bytes(),
            deadline,
            crate::http::request::RequestMethods::Standard,
        )
        .await
        .ok()
        .expect("valid request head")
        .raw
    }

    #[tokio::test]
    async fn reserves_wire_content_length_without_geometric_growth() {
        let target_length = 1024 * 1024 + 269;
        let raw = json_head(target_length).await;
        let limits =
            RequestLimits::new(RequestLimitsConfig::default(), || None, || 0, "body-test").unwrap();
        let mut config = prepare_json(raw.request(), &limits)
            .ok()
            .expect("valid declared length");
        let mut body = Vec::new();
        let result = read_json_body(
            axum::body::Body::from(vec![7_u8; 1024]),
            &raw,
            &mut config,
            &mut body,
            Vec::try_reserve_exact,
        )
        .await;
        assert!(matches!(result, Err(BodyError::Invalid(_))));
        assert_eq!(body, vec![7_u8; 1024]);
        assert_eq!(body.capacity(), target_length);
    }

    #[tokio::test]
    async fn allocation_failure_maps_to_safe_memory_capacity_error() {
        let length = isize::MAX as usize + 2;
        let raw = json_head(length).await;
        let mut config = JsonRead {
            length,
            limit: length,
            budget: None,
        };
        let mut bytes = Vec::new();
        let result = read_json_body(
            axum::body::Body::empty(),
            &raw,
            &mut config,
            &mut bytes,
            Vec::try_reserve_exact,
        )
        .await;
        assert_allocation_error(result, isize::MAX as usize + 1 + 16 * 1024 * 1024);
    }

    #[tokio::test]
    async fn ordinary_json_allocation_failure_reports_a_rounded_limit() {
        let length = 1024;
        let raw = json_head(length).await;
        let limits =
            RequestLimits::new(RequestLimitsConfig::default(), || None, || 0, "body-test").unwrap();
        let mut config = prepare_json(raw.request(), &limits)
            .ok()
            .expect("valid declared length");
        let result = read_json_body(
            axum::body::Body::empty(),
            &raw,
            &mut config,
            &mut Vec::new(),
            |bytes, requested| {
                assert_eq!(requested, length);
                bytes.try_reserve_exact(usize::MAX)
            },
        )
        .await;
        assert_allocation_error(result, 16 * 1024 * 1024);
    }

    fn assert_allocation_error(result: Result<(), BodyError>, limit: usize) {
        let Err(error @ BodyError::Capacity(_)) = result else {
            panic!("allocation failure must map to capacity error");
        };
        let reply = EntryError::from(error).reply().encode();
        assert!(reply.starts_with(b"HTTP/1.1 503"));
        let detail = &json_body(&reply)["error"];
        assert_eq!(detail["reason"], "memory_limit");
        assert_eq!(detail["limit_bytes"], limit);
    }
}
