//! Complete native Responses HTTP forwarding over the shared connection pool.
//! A request is projected once; retries retain that projection and credential
//! snapshot except for Python's explicit account refresh/effort fallback.

use crate::native_metadata::{native_response_headers, rewrite_native_model_event};
use crate::{
    MAX_UPSTREAM_BODY_BYTES, MAX_UPSTREAM_ERROR_BYTES, ProjectionIds, RouterError, RouterErrorKind,
    endpoint, response_json_stream_events, retry_after,
};
use emp_core::{Dialect, Protocol, ResolvedRoute};
use emp_protocol::collaboration::{
    CollaborationError, prepare_collaboration, restore_collaboration,
};
use emp_protocol::context_error::is_explicit_context_error;
use emp_protocol::native_responses::{NativeProjectionError, project_request};
use emp_protocol::portable_responses::validate_responses_body;
use emp_transport::{
    FailureClass, HttpClient, HttpFailureInput, HttpMethod, HttpResponse, HttpTransportErrorKind,
    http_failure, public_failure_message, status_error_class, zstd_encode,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use tokio::time::{Instant, timeout_at};

pub struct NativeCompleteResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: BTreeMap<String, String>,
}

pub struct NativeWebSocketPlan {
    pub url: String,
    pub payload: Value,
    pub headers: BTreeMap<String, String>,
    pub requested_model: String,
    pub upstream_model: String,
    pub plaintext_collaboration: bool,
}

impl NativeWebSocketPlan {
    pub fn project_event(&self, event: &Value) -> Result<Value, NativeHttpError> {
        let mut projected =
            rewrite_native_model_event(event, &self.requested_model, &self.upstream_model);
        if self.plaintext_collaboration {
            projected = restore_collaboration(&projected).map_err(collaboration_error)?;
        }
        Ok(projected)
    }
}

#[derive(Debug)]
pub struct NativeStreamEvent {
    pub event: String,
    pub body: Value,
    pub frame: Vec<u8>,
}

pub struct NativeStream {
    pub request_started: std::time::Instant,
    /// Opaque hash of the credential owner selected by the application.
    pub usage_owner: Option<String>,
    response: Option<HttpResponse>,
    requested_model: String,
    upstream_model: String,
    plaintext_collaboration: bool,
    declared_sse: bool,
    line_buffer: Vec<u8>,
    pending_wire: Vec<u8>,
    pending_data: Vec<Vec<u8>>,
    pending: VecDeque<NativeStreamEvent>,
    raw_body: Vec<u8>,
    stream_bytes: usize,
    saw_data: bool,
    saw_terminal: bool,
    finished: bool,
    failure: Option<RouterError>,
    ids: ProjectionIds,
    pub headers: BTreeMap<String, String>,
}

const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

fn native_stream_error(
    status: u16,
    class: FailureClass,
    reason: Option<&str>,
    message: &'static str,
) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        status,
        class,
        reason.map(str::to_owned),
        None,
        message,
    )
}

fn native_sse_frame(event: &str, body: &Value) -> Result<Vec<u8>, RouterError> {
    let encoded = serde_json::to_vec(body).map_err(|_| {
        native_stream_error(
            500,
            FailureClass::StreamError,
            None,
            "native stream event serialization failed",
        )
    })?;
    let mut frame = Vec::with_capacity(event.len() + encoded.len() + 16);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    frame.extend_from_slice(&encoded);
    frame.extend_from_slice(b"\n\n");
    Ok(frame)
}

impl NativeStream {
    pub async fn next_event(&mut self) -> Result<Option<NativeStreamEvent>, RouterError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(Some(event));
        }
        if let Some(error) = self.failure.take() {
            return Err(error);
        }
        if self.finished {
            return Ok(None);
        }
        loop {
            let next = self
                .response
                .as_mut()
                .ok_or_else(|| {
                    native_stream_error(
                        500,
                        FailureClass::StreamError,
                        None,
                        "native stream response is unavailable",
                    )
                })?
                .next_chunk()
                .await
                .map_err(crate::transport_error);
            let result = match next {
                Ok(Some(chunk)) => self.consume_chunk(&chunk),
                Ok(None) => self.consume_eof(),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                self.response.take();
                self.finished = true;
                if self.pending.is_empty() {
                    return Err(error);
                }
                self.failure = Some(error);
            }
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if let Some(error) = self.failure.take() {
                return Err(error);
            }
            if self.finished {
                return Ok(None);
            }
        }
    }

    pub async fn finish(mut self) {
        if let Some(response) = self.response.take() {
            response.finish().await;
        }
    }

    fn consume_chunk(&mut self, chunk: &[u8]) -> Result<(), RouterError> {
        self.stream_bytes = self.stream_bytes.checked_add(chunk.len()).ok_or_else(|| {
            native_stream_error(
                502,
                FailureClass::ProtocolError,
                Some("upstream_body_too_large"),
                "upstream stream is too large",
            )
        })?;
        if self.stream_bytes > MAX_UPSTREAM_BODY_BYTES {
            return Err(native_stream_error(
                502,
                FailureClass::ProtocolError,
                Some("upstream_body_too_large"),
                "upstream stream is too large",
            ));
        }
        if !self.saw_data {
            self.raw_body.extend_from_slice(chunk);
        }
        self.line_buffer.extend_from_slice(chunk);
        while let Some(position) = self.line_buffer.iter().position(|byte| *byte == b'\n') {
            let wire = self.line_buffer.drain(..=position).collect::<Vec<_>>();
            self.consume_line(&wire[..wire.len() - 1], &wire)?;
            if self.saw_data {
                self.raw_body.clear();
            }
        }
        if self.line_buffer.len() + self.pending_wire.len() > MAX_SSE_FRAME_BYTES {
            return Err(native_stream_error(
                502,
                FailureClass::MalformedTerminal,
                Some("sse_event_too_large"),
                "upstream Responses SSE event is too large",
            ));
        }
        Ok(())
    }

    fn consume_eof(&mut self) -> Result<(), RouterError> {
        if !self.line_buffer.is_empty() {
            let line = std::mem::take(&mut self.line_buffer);
            self.consume_line(&line, &line)?;
        }
        if !self.pending_data.is_empty() {
            self.consume_line(&[], &[])?;
        }
        if !self.saw_data && !self.declared_sse && !self.raw_body.is_empty() {
            let value: Value = serde_json::from_slice(&self.raw_body).map_err(|_| {
                native_stream_error(
                    502,
                    FailureClass::ProtocolError,
                    Some("invalid_upstream_json"),
                    "upstream Responses stream was neither SSE nor valid JSON",
                )
            })?;
            if is_explicit_context_error(200, "application/json", &self.raw_body) {
                return Err(native_stream_error(
                    413,
                    FailureClass::ContextLengthExceeded,
                    None,
                    "context length exceeded",
                ));
            }
            for event in response_json_stream_events(value, &self.ids, false)? {
                self.push_event(event, None)?;
            }
        }
        if !self.saw_terminal {
            return Err(native_stream_error(
                502,
                FailureClass::StreamIncomplete,
                Some("stream_incomplete"),
                "upstream Responses stream ended before response.completed",
            ));
        }
        self.response.take();
        self.finished = true;
        Ok(())
    }

    fn consume_line(&mut self, raw_line: &[u8], wire_line: &[u8]) -> Result<(), RouterError> {
        self.pending_wire.extend_from_slice(wire_line);
        if self.pending_wire.len() > MAX_SSE_FRAME_BYTES {
            return Err(native_stream_error(
                502,
                FailureClass::MalformedTerminal,
                Some("sse_event_too_large"),
                "upstream Responses SSE event is too large",
            ));
        }
        let line = std::str::from_utf8(raw_line)
            .map_err(|_| {
                native_stream_error(
                    502,
                    FailureClass::ProtocolError,
                    Some("sse_invalid_utf8"),
                    "upstream Responses stream is not valid UTF-8",
                )
            })?
            .trim_end_matches('\r');
        if let Some(data) = line.strip_prefix("data:") {
            self.saw_data = true;
            self.pending_data
                .push(data.trim_start().as_bytes().to_vec());
            return Ok(());
        }
        if !line.is_empty() {
            return Ok(());
        }
        if self.pending_data.is_empty() {
            self.pending_wire.clear();
            return Ok(());
        }
        let data = self.pending_data.split_off(0).join(&b'\n');
        let wire = std::mem::take(&mut self.pending_wire);
        if data == b"[DONE]" {
            return Ok(());
        }
        let Ok(event) = serde_json::from_slice::<Value>(&data) else {
            return Ok(());
        };
        if !event.is_object() {
            return Ok(());
        }
        if is_explicit_context_error(400, "application/json", &data) {
            return Err(native_stream_error(
                413,
                FailureClass::ContextLengthExceeded,
                None,
                "context length exceeded",
            ));
        }
        self.push_event(event, Some(wire))
    }

    fn push_event(
        &mut self,
        event: Value,
        original_frame: Option<Vec<u8>>,
    ) -> Result<(), RouterError> {
        let mut projected =
            rewrite_native_model_event(&event, &self.requested_model, &self.upstream_model);
        let model_changed = projected != event;
        if self.plaintext_collaboration {
            projected = restore_collaboration(&projected).map_err(|error| match error {
                CollaborationError::UnexpectedEncryptedArguments => native_stream_error(
                    400,
                    FailureClass::RouterError,
                    None,
                    "unexpected encrypted collaboration arguments",
                ),
                _ => native_stream_error(
                    500,
                    FailureClass::StreamError,
                    None,
                    "collaboration stream restoration failed",
                ),
            })?;
        }
        let event_type = projected
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_owned();
        if matches!(
            event_type.as_str(),
            "response.completed" | "response.incomplete"
        ) && let Some(response) = projected
            .get("response")
            .filter(|response| response.get("output").is_some())
        {
            validate_responses_body(response, false).map_err(|_| {
                native_stream_error(
                    502,
                    FailureClass::MalformedTerminal,
                    Some("invalid_terminal_response"),
                    "native terminal response is invalid",
                )
            })?;
        }
        if matches!(
            event_type.as_str(),
            "response.completed" | "response.incomplete" | "response.failed" | "error"
        ) {
            self.saw_terminal = true;
        }
        let frame = if !model_changed && !self.plaintext_collaboration {
            original_frame.unwrap_or(native_sse_frame(&event_type, &projected)?)
        } else {
            native_sse_frame(&event_type, &projected)?
        };
        self.pending.push_back(NativeStreamEvent {
            event: event_type,
            body: projected,
            frame,
        });
        Ok(())
    }
}

pub struct NativeHttpError {
    pub status: u16,
    pub body: Value,
    pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for NativeHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeHttpError")
            .field("status", &self.status)
            .field("body", &self.body)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl NativeHttpError {
    fn classified(
        status: u16,
        class: FailureClass,
        message: impl Into<String>,
        reason: Option<&str>,
        retry: Option<u64>,
    ) -> Self {
        let code = if class == FailureClass::RateLimit {
            "rate_limit_exceeded"
        } else {
            reason.unwrap_or(class.as_str())
        };
        let mut error = json!({"code": code, "type": class.as_str(), "message": message.into()});
        if let Some(reason) = reason {
            error["failure_reason"] = reason.into();
        }
        let mut headers = BTreeMap::new();
        if let Some(retry) = retry {
            error["retry_after_seconds"] = retry.into();
            headers.insert("Retry-After".to_owned(), retry.to_string());
        }
        Self {
            status,
            body: json!({"error": error}),
            headers,
        }
    }

    pub fn router(status: u16, message: impl Into<String>) -> Self {
        Self::classified(
            status,
            status_error_class(Some(status)),
            message,
            None,
            None,
        )
    }

    fn plain(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({"error": {"message":message.into()}}),
            headers: BTreeMap::new(),
        }
    }

    fn transport(class: FailureClass, status: u16, reason: Option<&str>) -> Self {
        Self::classified(
            status,
            class,
            format!("transport failure: class={}", class.as_str()),
            reason,
            None,
        )
    }

    fn context(headers: BTreeMap<String, String>) -> Self {
        let mut error = Self::classified(
            413,
            FailureClass::ContextLengthExceeded,
            "context length exceeded: estimated input unknown tokens, safe input limit unknown; provider unknown, model unknown; next action: reduce input or use native remote compaction",
            None,
            None,
        );
        error.headers = headers;
        error
    }
}

fn projection_error(error: NativeProjectionError) -> NativeHttpError {
    match error {
        NativeProjectionError::Projection(error)
            if error.failure_class() == "invalid_compaction" =>
        {
            NativeHttpError {
                status: 409,
                body: json!({"error": {"code":"history_reconstruction_failed", "message":"History reconstruction failed. Continue in the original task or start a new task.",
                "error_class":"history_reconstruction_failed", "reason":"history_projection_incomplete"}}),
                headers: BTreeMap::new(),
            }
        }
        NativeProjectionError::Projection(error) => NativeHttpError::router(422, error.to_string()),
        NativeProjectionError::UnhashableItemType => {
            NativeHttpError::plain(500, "internal server error")
        }
    }
}

fn collaboration_error(error: CollaborationError) -> NativeHttpError {
    match error {
        CollaborationError::NamespaceCollision => NativeHttpError::classified(
            422,
            FailureClass::RouterError,
            error.to_string(),
            error.failure_reason(),
            None,
        ),
        CollaborationError::UnexpectedEncryptedArguments => {
            NativeHttpError::plain(400, error.to_string())
        }
        _ => NativeHttpError::plain(500, "internal server error"),
    }
}

fn encoded(payload: &Value) -> Result<Vec<u8>, NativeHttpError> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|_| NativeHttpError::plain(500, "internal server error"))?;
    zstd_encode(&bytes)
        .map_err(|_| NativeHttpError::router(502, "native request compression failed"))
}

fn selected_headers(response: &HttpResponse, route: &ResolvedRoute) -> BTreeMap<String, String> {
    let values = response
        .headers()
        .map(|(name, value)| (name.to_owned(), Value::String(value.to_owned())))
        .collect::<Map<_, _>>();
    native_response_headers(
        &json!({"headers":values}),
        &route.requested_model,
        &route.upstream_model,
    )
    .into_iter()
    .filter_map(|(name, value)| value.as_str().map(|value| (name, value.to_owned())))
    .collect()
}

fn read_error(kind: HttpTransportErrorKind) -> NativeHttpError {
    match kind {
        HttpTransportErrorKind::ResponseTooLarge => {
            NativeHttpError::router(502, "upstream Responses 响应 is too large")
        }
        HttpTransportErrorKind::ReadTimeout => {
            NativeHttpError::router(504, "upstream request timed out")
        }
        _ => NativeHttpError::plain(500, "internal server error"),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn error_detail(content_type: &str, raw: &[u8]) -> (String, String) {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let decoded = String::from_utf8_lossy(raw);
    let stripped = decoded.trim_start().to_lowercase();
    if matches!(media.as_str(), "text/html" | "application/xhtml+xml")
        || stripped.starts_with("<html")
        || stripped.starts_with("<!doctype html")
    {
        return (
            if media.is_empty() {
                "text/html".to_owned()
            } else {
                media
            },
            "HTML error page omitted; the upstream gateway or WAF may have rejected the request"
                .to_owned(),
        );
    }
    let mut detail = decoded.to_string();
    if (media.contains("json") || stripped.starts_with(['{', '[']))
        && let Ok(value) = serde_json::from_str::<Value>(&decoded)
    {
        let nested = if value.is_object() {
            value
                .get("error")
                .filter(|value| truthy(value))
                .or_else(|| value.get("message").filter(|value| truthy(value)))
                .unwrap_or(&value)
        } else {
            &value
        };
        let nested = if nested.is_object() {
            ["message", "type", "code"]
                .iter()
                .find_map(|key| nested.get(key).filter(|value| truthy(value)))
                .unwrap_or(nested)
        } else {
            nested
        };
        detail = nested
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| nested.to_string());
    }
    detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.chars().count() > 512 {
        detail = detail
            .chars()
            .take(509)
            .collect::<String>()
            .trim_end()
            .to_owned()
            + "...";
    }
    (
        if media.is_empty() {
            "unknown content type".to_owned()
        } else {
            media
        },
        detail,
    )
}

fn proxy_evidence(headers: &str, detail: &str) -> bool {
    let text = format!("{headers} {detail}").to_lowercase();
    [
        "cannot connect to proxy",
        "proxy connect",
        "proxy connection",
        "proxy error",
        "proxy server",
        "tunnel connection failed",
        "proxy-agent",
        "x-squid-error",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn upstream_http_error(
    status: u16,
    content_type: &str,
    raw: &[u8],
    proxy_headers: &str,
    retry: Option<u64>,
    mut selected: BTreeMap<String, String>,
) -> NativeHttpError {
    let (_, detail) = error_detail(content_type, raw);
    let failure = http_failure(HttpFailureInput {
        status,
        detail: &detail,
        proxy_evidence: proxy_evidence(proxy_headers, &detail),
        retry_after_seconds: retry,
    });
    let reason = failure.failure_reason.as_deref().map(|reason| {
        if reason == "upstream_504" {
            "upstream_rejected"
        } else {
            reason
        }
    });
    let message = public_failure_message(failure.error_class, reason, failure.status);
    let mut error = NativeHttpError::classified(
        failure.status,
        failure.error_class,
        message,
        reason,
        if failure.error_class == FailureClass::ProxyUnavailable {
            None
        } else {
            failure.retry_after_seconds
        },
    );
    selected.extend(error.headers);
    error.headers = selected;
    error
}

fn compact_endpoint(provider: &Map<String, Value>) -> Result<String, NativeHttpError> {
    let base = provider
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| NativeHttpError::router(400, "provider base URL is missing"))?
        .trim_end_matches('/');
    Ok(if base.ends_with("/responses/compact") {
        base.to_owned()
    } else if base.ends_with("/responses") {
        format!("{base}/compact")
    } else {
        format!("{base}/responses/compact")
    })
}

pub struct NativeRouter<'a> {
    client: &'a HttpClient,
}

impl<'a> NativeRouter<'a> {
    pub fn new(client: &'a HttpClient) -> Self {
        Self { client }
    }

    pub fn prepare_websocket(
        &self,
        route: &ResolvedRoute,
        body: &Map<String, Value>,
        plaintext_collaboration: bool,
        headers: BTreeMap<String, String>,
    ) -> Result<NativeWebSocketPlan, NativeHttpError> {
        if route.dialect != Dialect::CodexNative || route.protocol != Protocol::Responses {
            return Err(NativeHttpError::router(
                503,
                "provider protocol is unsupported",
            ));
        }
        let mut payload = project_request(body).map_err(projection_error)?;
        payload["model"] = route.upstream_model.clone().into();
        if plaintext_collaboration {
            payload = prepare_collaboration(payload.as_object().expect("projected object"))
                .map_err(collaboration_error)?
                .0;
        }
        payload["type"] = Value::String("response.create".to_owned());
        payload
            .as_object_mut()
            .expect("projected object")
            .remove("stream");
        let endpoint = endpoint(route.provider.value(), Protocol::Responses)
            .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))?;
        let mut parsed = url::Url::parse(&endpoint).map_err(|_| {
            NativeHttpError::router(502, "native upstream websocket endpoint is invalid")
        })?;
        let scheme = match parsed.scheme() {
            "http" => "ws",
            "https" => "wss",
            "ws" => "ws",
            "wss" => "wss",
            _ => {
                return Err(NativeHttpError::router(
                    502,
                    "native upstream websocket endpoint is invalid",
                ));
            }
        };
        parsed.set_scheme(scheme).map_err(|_| {
            NativeHttpError::router(502, "native upstream websocket endpoint is invalid")
        })?;
        Ok(NativeWebSocketPlan {
            url: parsed.into(),
            payload,
            headers,
            requested_model: route.requested_model.clone(),
            upstream_model: route.upstream_model.clone(),
            plaintext_collaboration,
        })
    }

    pub async fn execute_complete<F>(
        &self,
        route: &ResolvedRoute,
        body: &Map<String, Value>,
        plaintext_collaboration: bool,
        allow_retries: bool,
        resolve_headers: F,
    ) -> Result<NativeCompleteResponse, NativeHttpError>
    where
        F: FnMut(bool) -> Result<BTreeMap<String, String>, NativeHttpError>,
    {
        self.execute_buffered(
            route,
            body,
            plaintext_collaboration,
            allow_retries,
            false,
            resolve_headers,
        )
        .await
    }

    pub async fn execute_compact<F>(
        &self,
        route: &ResolvedRoute,
        body: &Map<String, Value>,
        plaintext_collaboration: bool,
        resolve_headers: F,
    ) -> Result<NativeCompleteResponse, NativeHttpError>
    where
        F: FnMut(bool) -> Result<BTreeMap<String, String>, NativeHttpError>,
    {
        self.execute_buffered(
            route,
            body,
            plaintext_collaboration,
            true,
            true,
            resolve_headers,
        )
        .await
    }

    /// Header resolution occurs after projection. `refresh=true` means the
    /// first account attempt returned 401; failures retain that original 401.
    async fn execute_buffered<F>(
        &self,
        route: &ResolvedRoute,
        body: &Map<String, Value>,
        plaintext_collaboration: bool,
        allow_retries: bool,
        compact: bool,
        mut resolve_headers: F,
    ) -> Result<NativeCompleteResponse, NativeHttpError>
    where
        F: FnMut(bool) -> Result<BTreeMap<String, String>, NativeHttpError>,
    {
        if route.dialect != Dialect::CodexNative || route.protocol != Protocol::Responses {
            return Err(NativeHttpError::router(
                503,
                "provider protocol is unsupported",
            ));
        }
        let mut payload = project_request(body).map_err(projection_error)?;
        payload["model"] = route.upstream_model.clone().into();
        if plaintext_collaboration {
            payload = prepare_collaboration(payload.as_object().expect("projected object"))
                .map_err(collaboration_error)?
                .0;
        }
        let mut data = encoded(&payload)?;
        let deadline = Instant::now() + self.client.policy().timeout_policy().non_stream_wall_clock;
        let url = if compact {
            compact_endpoint(route.provider.value())?
        } else {
            endpoint(route.provider.value(), Protocol::Responses)
                .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))?
        };
        let mut headers: Option<BTreeMap<String, String>> = None;
        for attempt in 0..2 {
            if Instant::now() >= deadline {
                return Err(NativeHttpError::transport(
                    FailureClass::LocalDeadline,
                    504,
                    None,
                ));
            }
            if headers.is_none() {
                headers = Some(resolve_headers(false)?);
            }
            let mut request_headers = headers.as_ref().expect("resolved headers").clone();
            request_headers.insert("Content-Encoding".to_owned(), "zstd".to_owned());
            let opened = timeout_at(
                deadline,
                self.client.open(
                    HttpMethod::Post,
                    &url,
                    request_headers,
                    Some(data.clone()),
                    false,
                ),
            )
            .await;
            let response = match opened {
                Ok(Ok(response)) => response,
                result => {
                    let kind = match result {
                        Ok(Err(error)) => error.kind(),
                        Err(_) => HttpTransportErrorKind::ConnectTimeout,
                        _ => unreachable!(),
                    };
                    if attempt == 0
                        && allow_retries
                        && matches!(
                            kind,
                            HttpTransportErrorKind::Network
                                | HttpTransportErrorKind::ConnectTimeout
                                | HttpTransportErrorKind::ReadTimeout
                        )
                    {
                        continue;
                    }
                    return Err(match kind {
                        HttpTransportErrorKind::ConnectTimeout
                        | HttpTransportErrorKind::ReadTimeout => {
                            NativeHttpError::transport(FailureClass::ConnectTimeout, 504, None)
                        }
                        HttpTransportErrorKind::Network => {
                            NativeHttpError::transport(FailureClass::Network, 503, Some("network"))
                        }
                        _ => NativeHttpError::router(502, "upstream request failed"),
                    });
                }
            };
            let status = response.status();
            let content_type = response.header("content-type").unwrap_or("").to_owned();
            let selected = selected_headers(&response, route);
            if status >= 400 {
                let retry = retry_after::parse(response.header("retry-after"));
                let proxy_headers = response
                    .headers()
                    .map(|(name, value)| {
                        if matches!(name, "server" | "via" | "proxy-agent" | "x-squid-error") {
                            format!("{name} {value}")
                        } else {
                            name.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                let raw = timeout_at(deadline, response.read_prefix(MAX_UPSTREAM_ERROR_BYTES))
                    .await
                    .map_err(|_| NativeHttpError::router(504, "upstream request timed out"))?
                    .map_err(|error| read_error(error.kind()))?;
                if is_explicit_context_error(status, &content_type, &raw) {
                    return Err(NativeHttpError::context(selected));
                }
                if allow_retries
                    && attempt == 0
                    && status == 401
                    && route
                        .provider
                        .value()
                        .get("auth_mode")
                        .and_then(Value::as_str)
                        == Some("account")
                    && let Ok(refreshed) = resolve_headers(true)
                {
                    headers = Some(refreshed);
                    continue;
                }
                if allow_retries
                    && attempt == 0
                    && status == 400
                    && payload.get("reasoning_effort").is_some()
                    && String::from_utf8_lossy(&raw).contains("reasoning_effort")
                {
                    payload
                        .as_object_mut()
                        .expect("request object")
                        .remove("reasoning_effort");
                    data = encoded(&payload)?;
                    headers = None;
                    continue;
                }
                return Err(upstream_http_error(
                    status,
                    &content_type,
                    &raw,
                    &proxy_headers,
                    retry,
                    selected,
                ));
            }
            let raw = timeout_at(deadline, response.read_limited(MAX_UPSTREAM_BODY_BYTES))
                .await
                .map_err(|_| NativeHttpError::router(504, "upstream request timed out"))?
                .map_err(|error| read_error(error.kind()))?;
            if is_explicit_context_error(status, &content_type, &raw) {
                return Err(NativeHttpError::context(BTreeMap::new()));
            }
            let body = if plaintext_collaboration && !compact {
                let value: Value = serde_json::from_slice(&raw).map_err(|_| {
                    NativeHttpError::plain(400, "upstream native response is not valid JSON")
                })?;
                let restored = restore_collaboration(&value).map_err(collaboration_error)?;
                serde_json::to_vec(&restored)
                    .map_err(|_| NativeHttpError::plain(500, "internal server error"))?
            } else {
                raw
            };
            return Ok(NativeCompleteResponse {
                status,
                content_type: if content_type.is_empty() {
                    "application/json".to_owned()
                } else {
                    content_type
                },
                body,
                headers: selected,
            });
        }
        Err(NativeHttpError::router(502, "upstream request failed"))
    }

    pub async fn open_stream<F>(
        &self,
        route: &ResolvedRoute,
        body: &Map<String, Value>,
        plaintext_collaboration: bool,
        ids: &ProjectionIds,
        mut resolve_headers: F,
    ) -> Result<NativeStream, NativeHttpError>
    where
        F: FnMut(bool) -> Result<BTreeMap<String, String>, NativeHttpError>,
    {
        let request_started = std::time::Instant::now();
        if route.dialect != Dialect::CodexNative || route.protocol != Protocol::Responses {
            return Err(NativeHttpError::router(
                503,
                "provider protocol is unsupported",
            ));
        }
        let mut payload = project_request(body).map_err(projection_error)?;
        payload["model"] = route.upstream_model.clone().into();
        if plaintext_collaboration {
            payload = prepare_collaboration(payload.as_object().expect("projected object"))
                .map_err(collaboration_error)?
                .0;
        }
        let data = encoded(&payload)?;
        let url = endpoint(route.provider.value(), Protocol::Responses)
            .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))?;
        let mut request_headers = resolve_headers(false)?;
        request_headers.insert("Content-Encoding".to_owned(), "zstd".to_owned());
        let response = self
            .client
            .open(
                HttpMethod::Post,
                &url,
                request_headers,
                Some(data.clone()),
                true,
            )
            .await
            .map_err(|error| match error.kind() {
                HttpTransportErrorKind::ConnectTimeout | HttpTransportErrorKind::ReadTimeout => {
                    NativeHttpError::transport(FailureClass::ConnectTimeout, 504, None)
                }
                HttpTransportErrorKind::Network => {
                    NativeHttpError::transport(FailureClass::Network, 503, Some("network"))
                }
                _ => NativeHttpError::router(502, "upstream request failed"),
            })?;
        let status = response.status();
        let content_type = response.header("content-type").unwrap_or("").to_owned();
        let selected = selected_headers(&response, route);
        if status >= 400 {
            let retry = retry_after::parse(response.header("retry-after"));
            let proxy_headers = response
                .headers()
                .map(|(name, value)| {
                    if matches!(name, "server" | "via" | "proxy-agent" | "x-squid-error") {
                        format!("{name} {value}")
                    } else {
                        name.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            let raw = response
                .read_prefix(MAX_UPSTREAM_ERROR_BYTES)
                .await
                .map_err(|error| read_error(error.kind()))?;
            if is_explicit_context_error(status, &content_type, &raw) {
                return Err(NativeHttpError::context(selected));
            }
            return Err(upstream_http_error(
                status,
                &content_type,
                &raw,
                &proxy_headers,
                retry,
                selected,
            ));
        }
        if let Some(length) = response.header("content-length") {
            let length = length.parse::<usize>().map_err(|_| {
                NativeHttpError::router(502, "upstream stream has invalid Content-Length")
            })?;
            if length > MAX_UPSTREAM_BODY_BYTES {
                return Err(NativeHttpError::router(502, "upstream stream is too large"));
            }
        }
        let declared_sse = content_type
            .to_ascii_lowercase()
            .contains("text/event-stream");
        Ok(NativeStream {
            request_started,
            usage_owner: None,
            response: Some(response),
            requested_model: route.requested_model.clone(),
            upstream_model: route.upstream_model.clone(),
            plaintext_collaboration,
            declared_sse,
            line_buffer: Vec::new(),
            pending_wire: Vec::new(),
            pending_data: Vec::new(),
            pending: VecDeque::new(),
            raw_body: Vec::new(),
            stream_bytes: 0,
            saw_data: false,
            saw_terminal: false,
            finished: false,
            failure: None,
            ids: ids.clone(),
            headers: selected,
        })
    }
}
