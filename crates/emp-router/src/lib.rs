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
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

pub mod discovery;
pub mod native_http;
pub mod native_metadata;
pub mod native_request;
pub mod official_registry;

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

fn observed_protocol(route: &ResolvedRoute) -> Option<Protocol> {
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
        let mut tools = ExternalTools::default();
        let prepared = tools.prepare(body).map_err(tool_request_error)?;
        let body = &prepared;
        let provider = route.provider.value();
        let endpoint = endpoint(provider, route.protocol)?;
        let headers = upstream_headers(provider, route.protocol, incoming)?;
        let payload = project_prepared_external_payload(route, body)?;
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
                .read_prefix(MAX_UPSTREAM_ERROR_BYTES)
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
            Protocol::Auto => return Err(unresolved_protocol()),
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
            body: tools
                .restore_response(projected)
                .map_err(tool_response_error)?,
        })
    }

    pub async fn open_stream(
        &self,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        ids: &ProjectionIds,
    ) -> Result<ExternalStream, RouterError> {
        validate_stream_request(route, body)?;
        let mut tools = ExternalTools::default();
        let prepared = tools.prepare(body).map_err(tool_request_error)?;
        let body = &prepared;
        let provider = route.provider.value();
        let endpoint = endpoint(provider, route.protocol)?;
        let mut headers = upstream_headers(provider, route.protocol, incoming)?;
        headers.insert("Accept".to_owned(), "text/event-stream".to_owned());
        let portable_body = body_with_supported_effort(route, body);
        let canonical_custom_names = custom_tool_names(body);
        let custom_name_refs = canonical_custom_names
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let (payload, projection) = match route.protocol {
            Protocol::Auto => return Err(unresolved_protocol()),
            Protocol::ChatCompletions => {
                let mut payload = responses_to_chat(&portable_body, &route.upstream_model)
                    .map_err(protocol_error)?;
                payload["stream"] = Value::Bool(true);
                payload["stream_options"] = serde_json::json!({"include_usage": true});
                let stream = ChatStream::new(&route.requested_model, ids.chat()?)
                    .with_custom_names(&custom_name_refs);
                (payload, StreamProjection::Chat(stream))
            }
            Protocol::AnthropicMessages => {
                let mut payload = responses_to_anthropic(&portable_body, &route.upstream_model)
                    .map_err(anthropic_error)?;
                payload["stream"] = Value::Bool(true);
                let stream = AnthropicStream::new(
                    &route.requested_model,
                    ids.anthropic(),
                    &custom_name_refs,
                );
                (payload, StreamProjection::Anthropic(stream))
            }
            Protocol::Responses => {
                let model = route.model.value();
                let preserve_summary = model
                    .get("_emp_preserve_reasoning_summary")
                    .and_then(Value::as_bool)
                    == Some(true);
                let preserve_state = model
                    .get("_emp_preserve_reasoning_state")
                    .and_then(Value::as_bool)
                    == Some(true);
                let mut payload =
                    project_portable_request(provider, &portable_body, preserve_state)
                        .map_err(portable_request_error)?;
                payload["model"] = Value::String(route.upstream_model.clone());
                payload["stream"] = Value::Bool(true);
                let names = portable_custom_tool_names(body).map_err(portable_request_error)?;
                (
                    payload,
                    StreamProjection::Responses {
                        projector: PortableStreamProjector::new(
                            names,
                            preserve_summary,
                            preserve_state,
                        ),
                        saw_terminal: false,
                    },
                )
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
            .open(HttpMethod::Post, &endpoint, headers, Some(encoded), true)
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let retry_after_seconds = response
            .header("retry-after")
            .and_then(|value| value.trim().parse::<u64>().ok());
        if !(200..300).contains(&status) {
            let raw = response
                .read_prefix(MAX_UPSTREAM_ERROR_BYTES)
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
        let declared_sse = response
            .header("content-type")
            .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
        if let Some(length) = response.header("content-length") {
            let length = length.parse::<usize>().map_err(|_| {
                RouterError::new(
                    RouterErrorKind::Protocol,
                    502,
                    FailureClass::ProtocolError,
                    Some("invalid_content_length".to_owned()),
                    None,
                    "upstream stream has invalid Content-Length",
                )
            })?;
            if length > MAX_UPSTREAM_BODY_BYTES {
                return Err(upstream_body_too_large());
            }
        }
        let mut stream = ExternalStream {
            response: Some(response),
            parser: Some(SseJsonParser::new()),
            projection,
            tools,
            pending: VecDeque::new(),
            raw_body: Vec::new(),
            stream_bytes: 0,
            saw_sse: false,
            declared_sse,
            finished: false,
            failure: None,
            ids: ids.clone(),
        };
        match &mut stream.projection {
            StreamProjection::Chat(projector) => {
                stream
                    .pending
                    .push_back(projector.start_event().map_err(protocol_error)?.into());
            }
            StreamProjection::Anthropic(projector) => {
                stream
                    .pending
                    .push_back(projector.start_event().map_err(anthropic_error)?.into());
            }
            StreamProjection::Responses { .. } => {}
        }
        Ok(stream)
    }
}

/// Project the exact upstream request judged by EMP's destination context guard.
pub fn project_external_payload(route: &ResolvedRoute, body: &Value) -> Result<Value, RouterError> {
    let prepared = ExternalTools::default()
        .prepare(body)
        .map_err(tool_request_error)?;
    project_prepared_external_payload(route, &prepared)
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
            responses_to_chat(&portable_body, &route.upstream_model).map_err(protocol_error)
        }
        Protocol::AnthropicMessages => {
            responses_to_anthropic(&portable_body, &route.upstream_model).map_err(anthropic_error)
        }
        Protocol::Responses => {
            let preserve_state = route
                .model
                .value()
                .get("_emp_preserve_reasoning_state")
                .and_then(Value::as_bool)
                == Some(true);
            let mut payload = project_portable_request(provider, &portable_body, preserve_state)
                .map_err(portable_request_error)?;
            payload["model"] = Value::String(route.upstream_model.clone());
            Ok(payload)
        }
    }
}

impl ExternalStream {
    pub async fn next_event(&mut self) -> Result<Option<StreamResponseEvent>, RouterError> {
        while let Some(event) = self.next_projected_event().await? {
            if let Some(body) = self
                .tools
                .restore_event(event.body)
                .map_err(tool_response_error)?
            {
                return Ok(Some(StreamResponseEvent {
                    event: event.event,
                    body,
                }));
            }
        }
        Ok(None)
    }

    async fn next_projected_event(&mut self) -> Result<Option<StreamResponseEvent>, RouterError> {
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
                .ok_or_else(|| invalid_request("stream response is no longer available"))?
                .next_chunk()
                .await
                .map_err(transport_error);
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

    pub fn is_finished(&self) -> bool {
        self.finished && self.pending.is_empty() && self.failure.is_none()
    }

    fn consume_chunk(&mut self, chunk: &[u8]) -> Result<(), RouterError> {
        self.stream_bytes = self
            .stream_bytes
            .checked_add(chunk.len())
            .ok_or_else(upstream_body_too_large)?;
        if self.stream_bytes > MAX_UPSTREAM_BODY_BYTES {
            return Err(upstream_body_too_large());
        }
        if !self.saw_sse {
            self.raw_body.extend_from_slice(chunk);
        }
        let frames = self
            .parser
            .as_mut()
            .expect("active stream parser")
            .push_frames(chunk)
            .map_err(sse_transport_error)?;
        if !frames.is_empty() {
            self.saw_sse = true;
            self.raw_body.clear();
        }
        self.consume_frames(frames)
    }

    fn consume_eof(&mut self) -> Result<(), RouterError> {
        let frames = self
            .parser
            .take()
            .expect("active stream parser")
            .finish_frames()
            .map_err(sse_transport_error)?;
        if !frames.is_empty() {
            self.saw_sse = true;
            self.raw_body.clear();
        }
        self.consume_frames(frames)?;
        if self.finished {
            return Ok(());
        }
        if !self.saw_sse && !self.raw_body.is_empty() && self.use_ordinary_body() {
            self.consume_ordinary_body()?;
        } else {
            self.finish_projection()?;
        }
        self.response.take();
        self.finished = true;
        Ok(())
    }

    fn use_ordinary_body(&self) -> bool {
        !matches!(&self.projection, StreamProjection::Responses { .. }) || !self.declared_sse
    }

    fn consume_frames(&mut self, frames: Vec<SseFrame>) -> Result<(), RouterError> {
        for frame in frames {
            if self.finished {
                break;
            }
            match frame {
                SseFrame::Done => match &mut self.projection {
                    StreamProjection::Chat(projector) => {
                        projector.mark_done();
                        self.finish_projection()?;
                    }
                    StreamProjection::Anthropic(_) => self.finish_projection()?,
                    StreamProjection::Responses { .. } => {}
                },
                SseFrame::Json(value) => self.consume_json(Value::Object(value), ChatFrame::Sse)?,
            }
        }
        Ok(())
    }

    fn consume_json(&mut self, value: Value, frame: ChatFrame) -> Result<(), RouterError> {
        match &mut self.projection {
            StreamProjection::Chat(projector) => {
                self.pending.extend(
                    projector
                        .push(&value, frame)
                        .map_err(protocol_error)?
                        .into_iter()
                        .map(StreamResponseEvent::from),
                );
            }
            StreamProjection::Anthropic(projector) => {
                self.pending.extend(
                    projector
                        .push(&value)
                        .map_err(anthropic_error)?
                        .into_iter()
                        .map(StreamResponseEvent::from),
                );
            }
            StreamProjection::Responses {
                projector,
                saw_terminal,
            } => {
                let Some(projected) = projector.project(&value).map_err(portable_response_error)?
                else {
                    return Ok(());
                };
                let event = projected
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                    .to_owned();
                if matches!(event.as_str(), "response.completed" | "response.incomplete") {
                    if let Some(response) = projected
                        .get("response")
                        .filter(|response| response.get("output").is_some())
                    {
                        validate_responses_body(response, true)
                            .map_err(responses_validation_error)?;
                    }
                    *saw_terminal = true;
                } else if matches!(event.as_str(), "response.failed" | "error") {
                    *saw_terminal = true;
                }
                self.pending.push_back(StreamResponseEvent {
                    event,
                    body: projected,
                });
            }
        }
        Ok(())
    }

    fn consume_ordinary_body(&mut self) -> Result<(), RouterError> {
        let value: Value = serde_json::from_slice(&self.raw_body).map_err(|_| {
            RouterError::new(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                Some("invalid_upstream_json".to_owned()),
                None,
                "upstream stream was neither SSE nor valid JSON",
            )
        })?;
        match &self.projection {
            StreamProjection::Responses { .. } => {
                validate_responses_body(&value, true).map_err(responses_validation_error)?;
                for event in response_json_stream_events(value, &self.ids, true)? {
                    self.consume_json(event, ChatFrame::Sse)?;
                }
                self.finish_projection()
            }
            _ => {
                self.consume_json(value, ChatFrame::Ordinary)?;
                self.finish_projection()
            }
        }
    }

    fn finish_projection(&mut self) -> Result<(), RouterError> {
        match &mut self.projection {
            StreamProjection::Chat(projector) => {
                self.pending.extend(
                    projector
                        .finish()
                        .map_err(protocol_error)?
                        .into_iter()
                        .map(StreamResponseEvent::from),
                );
            }
            StreamProjection::Anthropic(projector) => {
                self.pending.extend(
                    projector
                        .finish()
                        .map_err(anthropic_error)?
                        .into_iter()
                        .map(StreamResponseEvent::from),
                );
            }
            StreamProjection::Responses { saw_terminal, .. } => {
                if !*saw_terminal {
                    return Err(RouterError::new(
                        RouterErrorKind::Protocol,
                        502,
                        FailureClass::StreamIncomplete,
                        Some("stream_incomplete".to_owned()),
                        None,
                        "upstream Responses stream ended before a terminal event",
                    ));
                }
            }
        }
        self.response.take();
        self.finished = true;
        Ok(())
    }
}

pub fn response_json_stream_events(
    mut response: Value,
    ids: &ProjectionIds,
    validate_output_items: bool,
) -> Result<Vec<Value>, RouterError> {
    validate_responses_body(&response, validate_output_items)
        .map_err(responses_validation_error)?;
    let root = response
        .as_object_mut()
        .expect("validated Responses object");
    root.entry("id".to_owned())
        .or_insert_with(|| Value::String(ids.response.clone()));
    root.entry("object".to_owned())
        .or_insert_with(|| Value::String("response".to_owned()));
    let status = root["status"]
        .as_str()
        .expect("validated status")
        .to_owned();
    let mut output = root["output"].as_array().expect("validated output").clone();
    if output.is_empty()
        && let Some(text) = root
            .get("output_text")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), str::to_owned)
            })
        && !text.is_empty()
    {
        output.push(serde_json::json!({
            "id": ids.message,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]
        }));
    }
    root.insert("output".to_owned(), Value::Array(output.clone()));
    let mut created_response = root.clone();
    created_response.insert("status".to_owned(), Value::String("in_progress".to_owned()));
    created_response.insert("output".to_owned(), Value::Array(Vec::new()));
    let mut events = vec![serde_json::json!({
        "type": "response.created",
        "response": created_response
    })];
    for (output_index, source) in output.iter().enumerate() {
        let mut item = source.as_object().cloned().ok_or_else(|| {
            RouterError::new(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                Some("invalid_response_output".to_owned()),
                None,
                "upstream Responses JSON contains an invalid output item",
            )
        })?;
        item.entry("id".to_owned()).or_insert_with(|| {
            Value::String(if output_index == 0 {
                ids.message.clone()
            } else {
                format!("item_{output_index}")
            })
        });
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            .to_owned();
        let mut added = item.clone();
        if item_type != "compaction" {
            added.insert("status".to_owned(), Value::String("in_progress".to_owned()));
        }
        events.push(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": output_index,
            "item": added
        }));
        if item_type == "message" {
            for (content_index, part) in item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Some(part) = part.as_object() else {
                    continue;
                };
                let Some((kind, field)) = (match part.get("type").and_then(Value::as_str) {
                    Some("output_text") => Some(("output_text", "text")),
                    Some("refusal") => Some(("refusal", "refusal")),
                    _ => None,
                }) else {
                    continue;
                };
                let item_id = item.get("id").cloned().unwrap_or(Value::Null);
                let value = part
                    .get(field)
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
                let mut empty = part.clone();
                empty.insert(field.to_owned(), Value::String(String::new()));
                events.push(serde_json::json!({
                    "type": "response.content_part.added", "item_id": item_id,
                    "output_index": output_index, "content_index": content_index,
                    "part": empty
                }));
                events.push(serde_json::json!({
                    "type": format!("response.{kind}.delta"), "item_id": item_id,
                    "output_index": output_index, "content_index": content_index,
                    "delta": value
                }));
                let mut done_event = Map::from_iter([
                    (
                        "type".to_owned(),
                        Value::String(format!("response.{kind}.done")),
                    ),
                    ("item_id".to_owned(), item_id.clone()),
                    ("output_index".to_owned(), Value::from(output_index)),
                    ("content_index".to_owned(), Value::from(content_index)),
                ]);
                done_event.insert(field.to_owned(), value);
                events.push(Value::Object(done_event));
                events.push(serde_json::json!({
                    "type": "response.content_part.done", "item_id": item_id,
                    "output_index": output_index, "content_index": content_index,
                    "part": part
                }));
            }
        } else if item_type == "function_call" {
            events.push(serde_json::json!({
                "type": "response.function_call_arguments.done",
                "item_id": item.get("id").cloned().unwrap_or(Value::Null),
                "output_index": output_index,
                "arguments": item.get("arguments").cloned().unwrap_or_else(|| Value::String("{}".to_owned()))
            }));
        }
        let mut done = item;
        if item_type != "compaction" {
            done.insert("status".to_owned(), Value::String("completed".to_owned()));
        }
        events.push(serde_json::json!({
            "type": "response.output_item.done",
            "output_index": output_index,
            "item": done
        }));
    }
    let terminal = match status.as_str() {
        "completed" => "response.completed",
        "incomplete" => "response.incomplete",
        "failed" => "response.failed",
        _ => unreachable!("validated Responses status"),
    };
    events.push(serde_json::json!({"type": terminal, "response": root}));
    Ok(events)
}

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

fn unresolved_protocol() -> RouterError {
    RouterError::new(
        RouterErrorKind::UnsupportedProtocol,
        501,
        FailureClass::ProtocolRejection,
        Some("protocol_not_negotiated".to_owned()),
        None,
        "automatic protocol must be negotiated before external routing",
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

fn tool_request_error(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::InvalidRequest,
        422,
        FailureClass::RouterError,
        None,
        None,
        message,
    )
}
fn tool_response_error(message: &'static str) -> RouterError {
    RouterError::new(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        None,
        None,
        message,
    )
}

pub use emp_protocol::context_error::is_explicit_context_error;
