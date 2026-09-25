//! Complete native Responses HTTP forwarding over the shared connection pool.
//! A request is projected once; retries retain that projection and credential
//! snapshot except for Python's explicit account refresh/effort fallback.

use crate::discovery::python_json_error_message;
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
use emp_protocol::native_responses::project_request;
use emp_protocol::portable_responses::validate_responses_body;
use emp_transport::{
    FailureClass, HttpClient, HttpMethod, HttpResponse, HttpTransportErrorKind, zstd_encode,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, VecDeque};
use tokio::time::{Instant, timeout_at};

mod error;
mod stream;

pub use error::NativeHttpError;
use error::{collaboration_error, projection_error, read_error, upstream_http_error};

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
                let value: Value = serde_json::from_slice(&raw).map_err(|error| {
                    NativeHttpError::plain(400, python_json_error_message(&raw, &error))
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
