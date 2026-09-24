//! Request orchestration for one immutable EMP route.
//!
//! This vertical slice executes complete and streamed external Responses,
//! Chat Completions, and Anthropic Messages requests over the native transport.

use emp_core::{Dialect, Protocol, ResolvedRoute};
use emp_protocol::anthropic_projection::{
    AnthropicError, AnthropicIds, AnthropicStream, response_from_anthropic, responses_to_anthropic,
};
use emp_protocol::portable_responses::{
    PortableProjectionError, PortableStreamProjector, ResponsesValidationError,
    custom_tool_names as portable_custom_tool_names, project_request as project_portable_request,
    project_response as project_portable_response, validate_responses_body,
};
use emp_protocol::tool_bridge::ExternalTools;
use emp_protocol::{
    ChatFrame, ChatIds, ChatStream, ProtocolError, StreamEvent, response_from_chat,
    responses_to_chat,
};
use emp_transport::{
    FailureClass, HttpClient, HttpFailureInput, HttpMethod, HttpResponse, HttpTransportError,
    HttpTransportErrorKind, SseFrame, SseJsonParser, TransportError, http_failure,
};
use serde_json::{Map, Value};
use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

pub mod discovery;
pub mod native_http;
pub mod native_metadata;
pub mod native_request;
pub mod official_registry;
mod retry_after;
pub mod subscription_catalog;

pub const MAX_UPSTREAM_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BYTES: usize = 4096;
const EMP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterErrorKind {
    InvalidRequest,
    MissingCredential,
    UnsupportedProtocol,
    Transport,
    Upstream,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterError {
    kind: RouterErrorKind,
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<String>,
    retry_after_seconds: Option<u64>,
    message: String,
}

impl RouterError {
    fn new(
        kind: RouterErrorKind,
        status: u16,
        error_class: FailureClass,
        failure_reason: Option<String>,
        retry_after_seconds: Option<u64>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            status,
            error_class,
            failure_reason,
            retry_after_seconds,
            message: message.into(),
        }
    }

    pub const fn kind(&self) -> RouterErrorKind {
        self.kind
    }

    pub const fn status(&self) -> u16 {
        self.status
    }

    pub const fn error_class(&self) -> FailureClass {
        self.error_class
    }

    pub fn failure_reason(&self) -> Option<&str> {
        self.failure_reason.as_deref()
    }

    pub const fn retry_after_seconds(&self) -> Option<u64> {
        self.retry_after_seconds
    }
}

impl fmt::Display for RouterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RouterError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionIds {
    response: String,
    message: String,
    reasoning: String,
    late_reasoning: String,
}

impl ProjectionIds {
    pub fn new(
        response: impl Into<String>,
        message: impl Into<String>,
        reasoning: impl Into<String>,
        late_reasoning: impl Into<String>,
    ) -> Self {
        Self {
            response: response.into(),
            message: message.into(),
            reasoning: reasoning.into(),
            late_reasoning: late_reasoning.into(),
        }
    }

    fn chat(&self) -> Result<ChatIds, RouterError> {
        ChatIds::new(
            self.response.clone(),
            self.message.clone(),
            self.reasoning.clone(),
            self.late_reasoning.clone(),
        )
        .map_err(protocol_error)
    }

    fn anthropic(&self) -> AnthropicIds {
        AnthropicIds::new(self.response.clone(), self.message.clone())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompleteResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamResponseEvent {
    pub event: String,
    pub body: Value,
}

impl From<StreamEvent> for StreamResponseEvent {
    fn from(value: StreamEvent) -> Self {
        Self {
            event: value.event.to_owned(),
            body: value.value,
        }
    }
}

#[derive(Debug)]
enum StreamProjection {
    Chat(ChatStream),
    Anthropic(AnthropicStream),
    Responses {
        projector: PortableStreamProjector,
        saw_terminal: bool,
    },
}

#[derive(Debug)]
pub struct ExternalStream {
    pub request_started: std::time::Instant,
    response: Option<HttpResponse>,
    parser: Option<SseJsonParser>,
    projection: StreamProjection,
    tools: ExternalTools,
    pending: VecDeque<StreamResponseEvent>,
    raw_body: Vec<u8>,
    stream_bytes: usize,
    saw_sse: bool,
    declared_sse: bool,
    finished: bool,
    failure: Option<RouterError>,
    ids: ProjectionIds,
}

pub struct ExternalRouter<'a> {
    client: &'a HttpClient,
}

/// Return Python-compatible concrete candidates for one frozen route.
///
/// A saved observation is trusted only when the endpoint, deployment and raw
/// upstream model identities still match. This function never mutates saved
/// configuration; persistence remains owned by the application state layer.
pub fn protocol_candidates(route: &ResolvedRoute) -> Vec<Protocol> {
    if route.protocol != Protocol::Auto {
        return vec![route.protocol];
    }
    let provider = route.provider.value();
    let normal = if provider.get("auth_mode").and_then(Value::as_str) == Some("anthropic_api_key") {
        vec![Protocol::AnthropicMessages]
    } else if provider
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('/')
        .ends_with("/responses")
    {
        vec![Protocol::Responses, Protocol::ChatCompletions]
    } else {
        vec![Protocol::ChatCompletions, Protocol::Responses]
    };
    let Some(observed) = observed_protocol(route) else {
        return normal;
    };
    if !normal.contains(&observed) || normal.first() == Some(&observed) {
        return normal;
    }
    std::iter::once(observed)
        .chain(
            normal
                .into_iter()
                .filter(|candidate| *candidate != observed),
        )
        .collect()
}

/// Return a saved protocol only when its endpoint, deployment, and upstream
/// model identities still match this frozen route. Candidate order based on a
/// URL suffix is not an observation and must not be treated as one.
pub fn observed_protocol(route: &ResolvedRoute) -> Option<Protocol> {
    let upstream = route
        .model
        .value()
        .get("upstream_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|upstream| !upstream.is_empty())?;
    for source in [route.model.value(), route.provider.value()] {
        let protocol = match source.get("resolved_protocol").and_then(Value::as_str) {
            Some("responses") => Protocol::Responses,
            Some("chat_completions") => Protocol::ChatCompletions,
            Some("anthropic_messages") => Protocol::AnthropicMessages,
            _ => continue,
        };
        let Some(observation) = source
            .get("protocol_observation")
            .and_then(Value::as_object)
        else {
            continue;
        };
        if observation
            .get("endpoint_fingerprint")
            .and_then(Value::as_str)
            != Some(route.endpoint_fingerprint.as_str())
            || observation
                .get("deployment_identity")
                .and_then(Value::as_str)
                != Some(route.deployment_identity.as_str())
            || observation.get("upstream_model").and_then(Value::as_str) != Some(upstream)
        {
            continue;
        }
        return Some(protocol);
    }
    None
}

/// Project the exact upstream request judged by EMP's destination context guard.
pub fn project_external_payload(route: &ResolvedRoute, body: &Value) -> Result<Value, RouterError> {
    let mut tools = ExternalTools::default();
    let prepared = tools.prepare_or_borrow(body).map_err(tool_request_error)?;
    project_prepared_external_payload(route, prepared.as_ref())
}

fn project_prepared_external_payload(
    route: &ResolvedRoute,
    body: &Value,
) -> Result<Value, RouterError> {
    let provider = route.provider.value();
    let portable_body = body_with_supported_effort(route, body);
    match route.protocol {
        Protocol::Auto => Err(unresolved_protocol()),
        Protocol::ChatCompletions => {
            responses_to_chat(portable_body.as_ref(), &route.upstream_model).map_err(protocol_error)
        }
        Protocol::AnthropicMessages => {
            responses_to_anthropic(portable_body.as_ref(), &route.upstream_model)
                .map_err(anthropic_error)
        }
        Protocol::Responses => {
            let preserve_state = route
                .model
                .value()
                .get("_emp_preserve_reasoning_state")
                .and_then(Value::as_bool)
                == Some(true);
            let mut payload =
                project_portable_request(provider, portable_body.as_ref(), preserve_state)
                    .map_err(portable_request_error)?;
            payload["model"] = Value::String(route.upstream_model.clone());
            Ok(payload)
        }
    }
}

mod errors;
mod external_route;
mod external_stream;
use errors::*;
pub use external_stream::response_json_stream_events;

fn upstream_body_too_large() -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        Some("upstream_body_too_large".to_owned()),
        None,
        "upstream stream is too large",
    )
}

fn sse_transport_error(error: TransportError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        Some(error.failure_reason().to_owned()),
        None,
        "upstream SSE framing failed",
    )
}

fn validate_complete_request(route: &ResolvedRoute, body: &Value) -> Result<(), RouterError> {
    let body = body
        .as_object()
        .ok_or_else(|| invalid_request("request body must be an object"))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| invalid_request("request.model is required"))?;
    if model != route.requested_model {
        return Err(invalid_request(
            "resolved route does not match request.model",
        ));
    }
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return Err(invalid_request(
            "complete routing does not accept a streamed request",
        ));
    }
    if !matches!(
        (route.dialect, route.protocol),
        (Dialect::PortableResponses, Protocol::Responses)
            | (Dialect::ChatCompletions, Protocol::ChatCompletions)
            | (Dialect::AnthropicMessages, Protocol::AnthropicMessages)
    ) {
        return Err(RouterError::new(
            RouterErrorKind::UnsupportedProtocol,
            501,
            FailureClass::ProtocolRejection,
            Some("unsupported_complete_dialect".to_owned()),
            None,
            "complete external dialect is not implemented",
        ));
    }
    Ok(())
}

fn validate_stream_request(route: &ResolvedRoute, body: &Value) -> Result<(), RouterError> {
    let body = body
        .as_object()
        .ok_or_else(|| invalid_request("request body must be an object"))?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| invalid_request("request.model is required"))?;
    if model != route.requested_model {
        return Err(invalid_request(
            "resolved route does not match request.model",
        ));
    }
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return Err(invalid_request("stream routing requires stream=true"));
    }
    if !matches!(
        (route.dialect, route.protocol),
        (Dialect::PortableResponses, Protocol::Responses)
            | (Dialect::ChatCompletions, Protocol::ChatCompletions)
            | (Dialect::AnthropicMessages, Protocol::AnthropicMessages)
    ) {
        return Err(RouterError::new(
            RouterErrorKind::UnsupportedProtocol,
            501,
            FailureClass::ProtocolRejection,
            Some("unsupported_stream_dialect".to_owned()),
            None,
            "streaming external dialect is not implemented",
        ));
    }
    Ok(())
}

fn body_with_supported_effort<'a>(route: &ResolvedRoute, body: &'a Value) -> Cow<'a, Value> {
    let Some(source) = body.as_object() else {
        return Cow::Borrowed(body);
    };
    let provider = route.provider.value();
    if matches!(
        provider.get("auth_mode").and_then(Value::as_str),
        Some("account" | "forward")
    ) {
        return Cow::Borrowed(body);
    }
    let Some(reasoning) = source.get("reasoning").and_then(Value::as_object) else {
        return Cow::Borrowed(body);
    };
    let Some(effort) = reasoning.get("effort") else {
        return Cow::Borrowed(body);
    };
    let model = route.model.value();
    let levels = model.get("reasoning_levels").and_then(Value::as_array);
    let persistent_alias = route.protocol == Protocol::Responses
        && effort.as_str() == Some("disabled")
        && levels.is_some_and(|levels| {
            levels
                .iter()
                .any(|level| level.as_str() == Some("persistent"))
        });
    let unsupported = model.get("supports_reasoning").and_then(Value::as_bool) == Some(false)
        || levels.is_some_and(|levels| {
            !levels.is_empty() && !levels.contains(effort) && !persistent_alias
        });
    if !unsupported {
        return Cow::Borrowed(body);
    }
    let mut projected = source.clone();
    let mut reasoning = reasoning.clone();
    reasoning.remove("effort");
    if reasoning.is_empty() {
        projected.remove("reasoning");
    } else {
        projected.insert("reasoning".to_owned(), Value::Object(reasoning));
    }
    Cow::Owned(Value::Object(projected))
}

fn endpoint(provider: &Map<String, Value>, protocol: Protocol) -> Result<String, RouterError> {
    let base = provider
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_request("provider base URL is missing"))?
        .trim_end_matches('/');
    let suffix = match protocol {
        Protocol::Auto => return Err(unresolved_protocol()),
        Protocol::Responses => "/responses",
        Protocol::ChatCompletions => "/chat/completions",
        Protocol::AnthropicMessages => "/messages",
    };
    Ok(if base.ends_with(suffix) {
        base.to_owned()
    } else {
        format!("{base}{suffix}")
    })
}

fn upstream_headers(
    provider: &Map<String, Value>,
    protocol: Protocol,
    incoming: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, RouterError> {
    let mut headers = BTreeMap::from([
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), format!("EMP/{EMP_VERSION}")),
    ]);
    if let Some(request_id) = incoming
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-emp-request-id"))
        .map(|(_, value)| value)
        .filter(|value| {
            value.len() == 16
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    {
        headers.insert("X-EMP-Request-ID".to_owned(), request_id.clone());
    }
    let auth_mode = provider
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or("api_key");
    let api_key = provider
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            RouterError::new(
                RouterErrorKind::MissingCredential,
                503,
                FailureClass::RouterError,
                Some("missing_api_key".to_owned()),
                None,
                "provider API key is not configured",
            )
        })?;
    match (protocol, auth_mode) {
        (Protocol::AnthropicMessages, "anthropic_api_key") => {
            headers.insert("x-api-key".to_owned(), api_key.to_owned());
            headers.insert(
                "anthropic-version".to_owned(),
                provider
                    .get("anthropic_version")
                    .and_then(Value::as_str)
                    .unwrap_or("2023-06-01")
                    .to_owned(),
            );
        }
        (_, "api_key") => {
            headers.insert("Authorization".to_owned(), format!("Bearer {api_key}"));
        }
        _ => {
            return Err(invalid_request(
                "provider authentication does not match protocol",
            ));
        }
    }
    Ok(headers)
}

fn custom_tool_names(body: &Value) -> Vec<String> {
    fn collect(value: Option<&Value>, names: &mut Vec<String>) {
        let Some(items) = value.and_then(Value::as_array) else {
            return;
        };
        for item in items {
            let Some(item) = item.as_object() else {
                continue;
            };
            if item.get("type").and_then(Value::as_str) == Some("namespace") {
                collect(item.get("tools"), names);
            } else if item.get("type").and_then(Value::as_str) == Some("custom")
                && let Some(name) = item.get("name").and_then(Value::as_str)
                && !name.is_empty()
                && !names.iter().any(|existing| existing == name)
            {
                names.push(name.to_owned());
            }
        }
    }
    let mut names = Vec::new();
    collect(body.get("tools"), &mut names);
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for item in input {
            if item.get("type").and_then(Value::as_str) == Some("additional_tools") {
                collect(item.get("tools"), &mut names);
            }
        }
    }
    names
}

pub use emp_protocol::context_error::is_explicit_context_error;

#[cfg(test)]
mod body_with_supported_effort_tests {
    use super::{Dialect, Protocol, body_with_supported_effort};
    use emp_core::{ResolvedRoute, RouteSource};
    use serde_json::json;
    use std::borrow::Cow;

    fn route() -> ResolvedRoute {
        ResolvedRoute::new(
            "demo/model",
            "upstream-model",
            RouteSource::ExplicitModel,
            json!({
                "id":"demo-provider", "base_url":"https://example.test/v1",
                "protocol":"responses", "auth_mode":"api_key", "api_key":"fixture"
            })
            .as_object()
            .expect("provider object")
            .clone(),
            json!({
                "id":"demo/model", "supports_reasoning":true,
                "reasoning_levels":["low"]
            })
            .as_object()
            .expect("model object")
            .clone(),
            Protocol::Responses,
            Dialect::PortableResponses,
            "demo-provider",
            format!("sha256:{}", "1".repeat(64)),
            "default",
        )
        .expect("resolved route")
    }

    #[test]
    fn supported_effort_returns_borrowed_body() {
        let route = route();
        let body = json!({
            "model":"demo/model", "input":"large request body",
            "reasoning":{"effort":"low"}
        });

        assert!(matches!(
            body_with_supported_effort(&route, &body),
            Cow::Borrowed(value) if std::ptr::eq(value, &body)
        ));
    }

    #[test]
    fn unsupported_effort_returns_owned_projected_body() {
        let route = route();
        let body = json!({
            "model":"demo/model", "input":"large request body",
            "reasoning":{"effort":"high", "summary":"auto"}
        });

        let projected = body_with_supported_effort(&route, &body);
        let Cow::Owned(projected) = projected else {
            panic!("unsupported effort must own the changed body");
        };
        assert_eq!(projected["input"], body["input"]);
        assert_eq!(projected["reasoning"]["summary"], "auto");
        assert!(projected["reasoning"].get("effort").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
    }
}
