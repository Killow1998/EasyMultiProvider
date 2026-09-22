//! Request orchestration for one immutable EMP route.
//!
//! This first vertical slice executes complete external Chat Completions and
//! Anthropic Messages requests. Portable Responses and streaming are kept out
//! until their independent projection/state-machine contracts are connected.

use emp_core::{Dialect, Protocol, ResolvedRoute};
use emp_protocol::anthropic_projection::{
    AnthropicError, AnthropicIds, response_from_anthropic, responses_to_anthropic,
};
use emp_protocol::portable_responses::{
    PortableProjectionError, ResponsesValidationError,
    custom_tool_names as portable_custom_tool_names, project_request as project_portable_request,
    project_response as project_portable_response, validate_responses_body,
};
use emp_protocol::{ChatIds, ProtocolError, response_from_chat, responses_to_chat};
use emp_transport::{
    FailureClass, HttpClient, HttpFailureInput, HttpMethod, HttpTransportError,
    HttpTransportErrorKind, http_failure,
};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

pub const MAX_UPSTREAM_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BYTES: usize = 64 * 1024;
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
    message: &'static str,
}

impl RouterError {
    fn new(
        kind: RouterErrorKind,
        status: u16,
        error_class: FailureClass,
        failure_reason: Option<String>,
        retry_after_seconds: Option<u64>,
        message: &'static str,
    ) -> Self {
        Self {
            kind,
            status,
            error_class,
            failure_reason,
            retry_after_seconds,
            message,
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
        formatter.write_str(self.message)
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

pub struct ExternalRouter<'a> {
    client: &'a HttpClient,
}

impl<'a> ExternalRouter<'a> {
    pub const fn new(client: &'a HttpClient) -> Self {
        Self { client }
    }

    pub async fn execute_complete(
        &self,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        ids: &ProjectionIds,
    ) -> Result<CompleteResponse, RouterError> {
        validate_complete_request(route, body)?;
        let provider = route.provider.value();
        let endpoint = endpoint(provider, route.protocol)?;
        let headers = upstream_headers(provider, route.protocol, incoming)?;
        let portable_body = body_with_supported_effort(route, body);
        let payload = match route.protocol {
            Protocol::ChatCompletions => {
                responses_to_chat(&portable_body, &route.upstream_model).map_err(protocol_error)?
            }
            Protocol::AnthropicMessages => {
                responses_to_anthropic(&portable_body, &route.upstream_model)
                    .map_err(anthropic_error)?
            }
            Protocol::Responses => {
                let preserve_state = route
                    .model
                    .value()
                    .get("_emp_preserve_reasoning_state")
                    .and_then(Value::as_bool)
                    == Some(true);
                let mut payload =
                    project_portable_request(provider, &portable_body, preserve_state)
                        .map_err(portable_request_error)?;
                payload["model"] = Value::String(route.upstream_model.clone());
                payload
            }
        };
        let encoded = serde_json::to_vec(&payload).map_err(|_| {
            RouterError::new(
                RouterErrorKind::InvalidRequest,
                422,
                FailureClass::RouterError,
                Some("request_serialization".to_owned()),
                None,
                "request serialization failed",
            )
        })?;
        let response = self
            .client
            .open(HttpMethod::Post, &endpoint, headers, Some(encoded), false)
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let content_type = response
            .header("content-type")
            .unwrap_or("application/json")
            .to_owned();
        let retry_after_seconds = response
            .header("retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok());
        if !(200..300).contains(&status) {
            let raw = response
                .read_limited(MAX_UPSTREAM_ERROR_BYTES)
                .await
                .map_err(transport_error)?;
            let detail = String::from_utf8_lossy(&raw);
            let failure = http_failure(HttpFailureInput {
                status,
                detail: &detail,
                proxy_evidence: false,
                retry_after_seconds,
            });
            return Err(RouterError::new(
                RouterErrorKind::Upstream,
                failure.status,
                failure.error_class,
                failure.failure_reason,
                failure.retry_after_seconds,
                "upstream request failed",
            ));
        }
        let raw = response
            .read_limited(MAX_UPSTREAM_BODY_BYTES)
            .await
            .map_err(transport_error)?;
        let upstream: Value = serde_json::from_slice(&raw).map_err(|_| {
            RouterError::new(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                Some("invalid_upstream_json".to_owned()),
                None,
                "upstream response is not valid JSON",
            )
        })?;
        let canonical_custom_names = custom_tool_names(body);
        let custom_name_refs = canonical_custom_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let projected = match route.protocol {
            Protocol::ChatCompletions => response_from_chat(
                &upstream,
                &route.requested_model,
                &custom_name_refs,
                &ids.chat()?,
            )
            .map(|projection| projection.response)
            .map_err(protocol_error)?,
            Protocol::AnthropicMessages => response_from_anthropic(
                &upstream,
                &route.requested_model,
                &custom_name_refs,
                &mut ids.anthropic(),
            )
            .map_err(anthropic_error)?,
            Protocol::Responses => {
                validate_responses_body(&upstream, true).map_err(responses_validation_error)?;
                let names = portable_custom_tool_names(body).map_err(portable_request_error)?;
                let model = route.model.value();
                let projected = project_portable_response(
                    &upstream,
                    &names,
                    model
                        .get("_emp_preserve_reasoning_summary")
                        .and_then(Value::as_bool)
                        == Some(true),
                    model
                        .get("_emp_preserve_reasoning_state")
                        .and_then(Value::as_bool)
                        == Some(true),
                )
                .map_err(portable_response_error)?;
                validate_responses_body(&projected, true).map_err(responses_validation_error)?;
                projected
            }
        };
        Ok(CompleteResponse {
            status,
            content_type,
            body: projected,
        })
    }
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

fn body_with_supported_effort(route: &ResolvedRoute, body: &Value) -> Value {
    let Some(source) = body.as_object() else {
        return body.clone();
    };
    let provider = route.provider.value();
    if matches!(
        provider.get("auth_mode").and_then(Value::as_str),
        Some("account" | "forward")
    ) {
        return body.clone();
    }
    let Some(reasoning) = source.get("reasoning").and_then(Value::as_object) else {
        return body.clone();
    };
    let Some(effort) = reasoning.get("effort") else {
        return body.clone();
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
        return body.clone();
    }
    let mut projected = source.clone();
    let mut reasoning = reasoning.clone();
    reasoning.remove("effort");
    if reasoning.is_empty() {
        projected.remove("reasoning");
    } else {
        projected.insert("reasoning".to_owned(), Value::Object(reasoning));
    }
    Value::Object(projected)
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

fn invalid_request(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        422,
        FailureClass::RouterError,
        Some("invalid_request".to_owned()),
        None,
        message,
    )
}

fn protocol_error(error: ProtocolError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        error.status(),
        match error.error_class() {
            "invalid_request" => FailureClass::RouterError,
            "stream_incomplete" => FailureClass::StreamIncomplete,
            _ => FailureClass::ProtocolError,
        },
        Some(error.error_class().to_owned()),
        None,
        error.message(),
    )
}

fn anthropic_error(error: AnthropicError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        error.status(),
        match error.error_class() {
            "invalid_request" => FailureClass::RouterError,
            "stream_incomplete" => FailureClass::StreamIncomplete,
            _ => FailureClass::ProtocolError,
        },
        Some(error.error_class().to_owned()),
        None,
        "Anthropic protocol projection failed",
    )
}

fn portable_request_error(error: PortableProjectionError) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        422,
        FailureClass::RouterError,
        Some(error.failure_class().to_owned()),
        None,
        "portable Responses request projection failed",
    )
}

fn portable_response_error(_error: PortableProjectionError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        None,
        None,
        "external Responses response projection failed",
    )
}

fn responses_validation_error(error: ResponsesValidationError) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        None,
        None,
        error.message(),
    )
}

fn transport_error(error: HttpTransportError) -> RouterError {
    let (status, error_class, reason) = match error.kind() {
        HttpTransportErrorKind::InvalidRequest => {
            (422, FailureClass::RouterError, "invalid_request")
        }
        HttpTransportErrorKind::ClientBuild => (503, FailureClass::Network, "client_build_failed"),
        HttpTransportErrorKind::ConnectTimeout => {
            (504, FailureClass::ConnectTimeout, "connect_timeout")
        }
        HttpTransportErrorKind::ReadTimeout => (504, FailureClass::Timeout, "read_timeout"),
        HttpTransportErrorKind::ResponseTooLarge => {
            (502, FailureClass::ProtocolError, "upstream_body_too_large")
        }
        HttpTransportErrorKind::Network => (503, FailureClass::Network, "network"),
        HttpTransportErrorKind::RedirectDisabled => {
            (502, FailureClass::ProtocolError, "redirect_disabled")
        }
    };
    RouterError::new(
        RouterErrorKind::Transport,
        status,
        error_class,
        Some(reason.to_owned()),
        None,
        "upstream transport failed",
    )
}
