use super::raw_head;
use crate::api::realtime::RealtimeError;
use crate::app::ServerState;
use crate::http::response::{
    SECURITY_HEADERS, is_security_header, json_error_body, response_with_headers, status_text,
};
use crate::server::http::body::BodyError;
use crate::services::error_presentation::ErrorPresentation;
use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header::CONTENT_LENGTH},
    middleware::Next,
    response::{IntoResponse, Response},
};
use emp_transport::{ContentDecodeError, RequestCapacityError};
use std::{borrow::Cow, sync::Arc};

pub(crate) enum EntryError {
    InvalidHead,
    NotFound,
    ManagementSession,
    CrossOrigin(&'static str),
    Json(u16, &'static str, Option<&'static str>),
    Updating,
    Body(BodyError),
    Realtime(RealtimeError),
    Recovery(u16, &'static str),
}

#[cfg_attr(test, derive(Debug))]
pub(crate) struct HttpReply {
    status: u16,
    status_text: &'static str,
    content_type: Cow<'static, str>,
    body: Bytes,
    extra_headers: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

pub(crate) type JsonReply = HttpReply;

#[derive(Clone)]
pub(in crate::server) struct EntryWire(pub(in crate::server) Bytes);

pub(super) async fn present_entry_errors(
    State(state): State<Arc<ServerState>>,
    request: Request,
    next: Next,
) -> Response {
    let presentation = raw_head(request.extensions()).ok().and_then(|raw| {
        let request = raw.request();
        request.is_model_http().then(|| {
            let mut presentation = ErrorPresentation::new(&state);
            presentation.thread(request.header("thread-id"));
            presentation
        })
    });
    let mut response = next.run(request).await;
    if let Some(presentation) = presentation
        && let Some(wire) = response.extensions_mut().get_mut::<EntryWire>()
        && let Some(formatted) = presentation.http(&wire.0)
    {
        let body_start = formatted
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .expect("presented HTTP response has a body")
            + 4;
        wire.0 = Bytes::from(formatted);
        let body = wire.0.slice(body_start..);
        response
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
        *response.body_mut() = Body::from(body);
    }
    response
}

impl HttpReply {
    pub(crate) fn new(status: u16, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            status_text: status_text(status),
            content_type: "application/json".into(),
            body: body.into(),
            extra_headers: Vec::new(),
        }
    }

    pub(crate) fn error(status: u16, message: &str, code: Option<&str>) -> Self {
        Self::new(status, json_error_body(message, code))
    }

    pub(crate) fn with_header(
        mut self,
        name: impl Into<Cow<'static, str>>,
        value: impl Into<Cow<'static, str>>,
    ) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    pub(crate) fn with_content_type(mut self, content_type: impl Into<Cow<'static, str>>) -> Self {
        self.content_type = content_type.into();
        self
    }

    pub(crate) fn with_status_text(mut self, status_text: &'static str) -> Self {
        self.status_text = status_text;
        self
    }

    pub(crate) fn into_http_response(self) -> Response {
        let mut builder = response_head(self.status, &self.content_type)
            .header("content-length", self.body.len())
            .extension(hyper::ext::ReasonPhrase::from_static(
                self.status_text.as_bytes(),
            ));
        if self.content_type == "application/json" {
            builder = builder.header("cache-control", "no-store");
        }
        for (name, value) in self.extra_headers {
            if !is_security_header(&name) {
                builder = builder.header(name.as_ref(), value.as_ref());
            }
        }
        builder
            .body(Body::from(self.body))
            .expect("EMP HTTP response")
    }

    pub(crate) fn present(&mut self, errors: &ErrorPresentation) {
        if let Some(body) = errors.http_body(self.status, &self.body) {
            self.body = body.into();
        }
    }

    pub(super) fn receipt(&self) -> super::inference::BufferedReceipt {
        super::inference::BufferedReceipt::Response {
            status: self.status,
            body: self.body.clone(),
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        response_with_headers(
            &format!("HTTP/1.1 {} {}", self.status, self.status_text),
            &self.content_type,
            &self.body,
            self.extra_headers
                .iter()
                .map(|(name, value)| (name.as_ref(), value.as_ref())),
        )
    }
}

pub(crate) fn response_head(status: u16, content_type: &str) -> axum::http::response::Builder {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).expect("EMP HTTP status"))
        .header("content-type", content_type)
        .header("connection", "close");
    for line in std::str::from_utf8(SECURITY_HEADERS)
        .expect("security headers")
        .lines()
    {
        let (name, value) = line.split_once(": ").expect("security header");
        builder = builder.header(name, value);
    }
    builder
}

pub(super) fn no_response() -> Response {
    let mut response = Response::new(Body::empty());
    response.extensions_mut().insert(EntryWire(Bytes::new()));
    response
}

impl IntoResponse for HttpReply {
    fn into_response(self) -> Response {
        let wire = EntryWire(Bytes::from(self.encode()));
        let mut response = self.into_http_response();
        response.extensions_mut().insert(wire);
        response
    }
}

impl EntryError {
    pub(crate) fn internal() -> Self {
        Self::Json(500, "internal server error", None)
    }

    pub(crate) fn reply(self) -> JsonReply {
        match self {
            Self::InvalidHead => plain(400, "invalid HTTP request"),
            Self::NotFound => plain(404, "not found"),
            Self::ManagementSession => plain(401, "management session is required"),
            Self::CrossOrigin(message) => plain(403, message),
            Self::Json(status, message, code) => JsonReply::error(status, message, code),
            Self::Updating => JsonReply::new(
                503,
                json_error_body(
                    "EMP is installing an update. Please retry shortly.",
                    Some("updating"),
                ),
            )
            .with_header("Retry-After", "2"),
            Self::Body(error) => match error {
                BodyError::Invalid(message) => JsonReply::new(400, json_error_body(&message, None)),
                BodyError::Timeout => JsonReply::new(
                    408,
                    json_error_body("request body timed out", Some("request_timeout")),
                ),
                BodyError::Capacity(error)
                | BodyError::Decode(ContentDecodeError::Capacity(error)) => capacity(&error),
                BodyError::Decode(error) => JsonReply::new(
                    error.http_status(),
                    json_error_body(
                        &error.to_string(),
                        (error.http_status() == 413).then_some("request_too_large"),
                    ),
                ),
            },
            Self::Realtime(error) => JsonReply::new(
                error.status,
                json_error_body(&error.message, Some(error.code)),
            ),
            Self::Recovery(status, code) => JsonReply::new(
                status,
                serde_json::json!({"error":code}).to_string().into_bytes(),
            ),
        }
    }
}

impl From<BodyError> for EntryError {
    fn from(error: BodyError) -> Self {
        Self::Body(error)
    }
}

impl From<RealtimeError> for EntryError {
    fn from(error: RealtimeError) -> Self {
        Self::Realtime(error)
    }
}

impl IntoResponse for EntryError {
    fn into_response(self) -> Response {
        self.reply().into_response()
    }
}

fn plain(status: u16, message: &str) -> JsonReply {
    JsonReply::new(
        status,
        serde_json::json!({"error":{"message":message}})
            .to_string()
            .into_bytes(),
    )
}

fn capacity(error: &RequestCapacityError) -> JsonReply {
    let unavailable = error.http_status() == 503;
    let code = if unavailable {
        "request_capacity_unavailable"
    } else {
        "request_too_large"
    };
    let mut detail = serde_json::json!({
        "code": code,
        "reason": error.reason,
        "message": error.to_string(),
        "limit_bytes": error.limit,
    });
    if unavailable {
        detail["memory"] = serde_json::json!({
            "used_percent": error.memory_used_percent,
            "used_bytes": error.memory_used_bytes,
            "total_bytes": error.memory_total_bytes,
            "available_bytes": error.available_bytes,
            "required_bytes": error.required_memory_bytes,
        });
    }
    let reply = JsonReply::new(
        error.http_status(),
        serde_json::json!({"error":detail}).to_string().into_bytes(),
    );
    if unavailable {
        reply.with_header("Retry-After", "2")
    } else {
        reply
    }
}
