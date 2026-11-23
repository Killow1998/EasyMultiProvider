use super::{error::EntryError, raw_head};
use crate::api::realtime::{
    RealtimeCall, RealtimeError,
    multipart::{RealtimeRead, validate_headers},
};
use crate::app::ServerState;
use crate::http::request::{
    REQUEST_READ_TIMEOUT, Request, read_exact_before, request_body_deadline, request_body_timeout,
    restore_read_timeout,
};
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
use std::{future::poll_fn, net::TcpStream, pin::Pin, sync::Arc};
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

fn reserve_body_capacity(
    body: &mut Vec<u8>,
    target_length: usize,
) -> Result<(), std::collections::TryReserveError> {
    body.try_reserve_exact(target_length.saturating_sub(body.len()))
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

pub(crate) fn read_json_body(
    stream: &mut TcpStream,
    request: Request<'_>,
    body: Vec<u8>,
    state: &ServerState,
) -> Result<Value, BodyError> {
    read_json_body_with_limits(
        stream,
        request,
        body,
        &state.backend.transport.request_limits,
    )
}

pub(super) fn read_json_body_with_limits(
    stream: &mut TcpStream,
    request: Request<'_>,
    mut body: Vec<u8>,
    limits: &Arc<RequestLimits>,
) -> Result<Value, BodyError> {
    let input = prepare_json(request, limits)?;
    body.truncate(input.length);
    reserve_body_capacity(&mut body, input.length)
        .map_err(|_| allocation_capacity_error(input.length))?;
    let deadline = request_body_deadline(input.length);
    let read = read_exact_before(stream, &mut body, input.length, deadline);
    restore_read_timeout(stream);
    read.map_err(|error| {
        if error.kind() == std::io::ErrorKind::TimedOut {
            BodyError::Timeout
        } else {
            BodyError::incomplete()
        }
    })?;
    decode_json(
        body,
        request.header("Content-Encoding").unwrap_or_default(),
        input,
    )
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
        let config = prepare_json(raw.request(), limits)?;
        let encoding = raw
            .request()
            .header("Content-Encoding")
            .unwrap_or_default()
            .to_owned();
        raw.allow_body();
        let mut config = config;
        let body = read_body(request.into_body(), config.length, |bytes, chunk| {
            let projected = bytes.len() + chunk.len();
            if projected > config.length {
                config.ensure(projected)?;
                return Err(BodyError::incomplete());
            }
            if projected > bytes.capacity() {
                let target = (bytes.capacity().saturating_mul(2))
                    .max(projected)
                    .min(config.length);
                config.ensure(target)?;
                reserve_body_capacity(bytes, target)
                    .map_err(|_| allocation_capacity_error(target))?;
            }
            bytes.extend_from_slice(chunk);
            Ok(())
        })
        .await?;
        let value = run_blocking(move || decode_json(body, &encoding, config))
            .await
            .map_err(|_| EntryError::internal())??;
        Ok(Self(value))
    }
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
        let bytes = read_body(
            request.into_body(),
            input.length,
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
    mut append: impl FnMut(&mut Vec<u8>, &[u8]) -> Result<(), E>,
) -> Result<Vec<u8>, E> {
    let deadline = Instant::now() + request_body_timeout(length);
    let mut bytes = Vec::new();
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
            append(&mut bytes, &chunk)?;
        }
    }
    if bytes.len() != length {
        return Err(BodyError::incomplete().into());
    }
    Ok(bytes)
}

#[cfg(test)]
mod body_capacity_tests {
    use super::{
        BodyError, REQUEST_GROWTH_QUANTUM, allocation_capacity_error, reserve_body_capacity,
    };
    use emp_transport::RequestCapacityReason;

    #[test]
    fn reserves_wire_content_length_without_geometric_growth() {
        let target_length = 1024 * 1024 + 269;
        let mut body = Vec::with_capacity(1024);
        body.extend_from_slice(&[0_u8; 1024]);

        reserve_body_capacity(&mut body, target_length).expect("reserve validated wire size");
        assert!(body.capacity() >= target_length);
        let reserved_capacity = body.capacity();
        let reserved_pointer = body.as_ptr();

        let chunk = [7_u8; 64 * 1024];
        while body.len() + chunk.len() <= target_length {
            body.extend_from_slice(&chunk);
        }
        let remainder = target_length - body.len();
        body.extend_from_slice(&chunk[..remainder]);
        assert_eq!(body.len(), target_length);
        assert_eq!(body.capacity(), reserved_capacity);
        assert_eq!(body.as_ptr(), reserved_pointer);
        assert!(body.iter().all(|byte| *byte == 0 || *byte == 7));
    }

    #[test]
    fn allocation_failure_maps_to_safe_memory_capacity_error() {
        let BodyError::Capacity(error) = allocation_capacity_error(1024) else {
            panic!("allocation failure must map to capacity error");
        };
        assert_eq!(error.reason, RequestCapacityReason::MemoryLimit);
        assert_eq!(error.http_status(), 503);
        assert_eq!(error.limit, REQUEST_GROWTH_QUANTUM);
    }
}
