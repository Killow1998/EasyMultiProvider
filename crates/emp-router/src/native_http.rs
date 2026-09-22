//! Complete native Responses HTTP forwarding over the shared connection pool.
//! A request is projected once; retries retain that projection and credential
//! snapshot except for Python's explicit account refresh/effort fallback.

use crate::native_metadata::native_response_headers;
use crate::{MAX_UPSTREAM_BODY_BYTES, MAX_UPSTREAM_ERROR_BYTES, endpoint};
use emp_core::{Dialect, Protocol, ResolvedRoute};
use emp_protocol::collaboration::{
    CollaborationError, prepare_collaboration, restore_collaboration,
};
use emp_protocol::context_error::is_explicit_context_error;
use emp_protocol::native_responses::{NativeProjectionError, project_request};
use emp_transport::{
    FailureClass, HttpClient, HttpFailureInput, HttpMethod, HttpResponse, HttpTransportErrorKind,
    http_failure, public_failure_message, status_error_class, zstd_encode,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt;
use tokio::time::{Instant, timeout_at};

pub struct NativeCompleteResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: BTreeMap<String, String>,
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

fn retry_after(value: Option<&str>) -> Option<u64> {
    let value = value.filter(|value| value.len() <= 128)?.trim();
    let delay = value.parse::<f64>().ok().or_else(|| {
        let date =
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc2822)
                .ok()?;
        Some(
            (date.unix_timestamp() as f64
                - time::OffsetDateTime::now_utc().unix_timestamp_nanos() as f64 / 1e9)
                .max(0.0),
        )
    })?;
    (delay.is_finite() && delay >= 0.0 && delay <= u64::MAX as f64).then(|| delay.ceil() as u64)
}

pub struct NativeRouter<'a> {
    client: &'a HttpClient,
}

impl<'a> NativeRouter<'a> {
    pub fn new(client: &'a HttpClient) -> Self {
        Self { client }
    }

    /// Header resolution occurs after projection. `refresh=true` means the
    /// first account attempt returned 401; failures retain that original 401.
    pub async fn execute_complete<F>(
        &self,
        route: &ResolvedRoute,
        body: &Map<String, Value>,
        plaintext_collaboration: bool,
        allow_retries: bool,
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
        let url = endpoint(route.provider.value(), Protocol::Responses)
            .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))?;
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
            let mut selected = selected_headers(&response, route);
            if status >= 400 {
                let retry = retry_after(response.header("retry-after"));
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
                let (_, detail) = error_detail(&content_type, &raw);
                let failure = http_failure(HttpFailureInput {
                    status,
                    detail: &detail,
                    proxy_evidence: proxy_evidence(&proxy_headers, &detail),
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
                return Err(error);
            }
            let raw = timeout_at(deadline, response.read_limited(MAX_UPSTREAM_BODY_BYTES))
                .await
                .map_err(|_| NativeHttpError::router(504, "upstream request timed out"))?
                .map_err(|error| read_error(error.kind()))?;
            if is_explicit_context_error(status, &content_type, &raw) {
                return Err(NativeHttpError::context(BTreeMap::new()));
            }
            let body = if plaintext_collaboration {
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
}
