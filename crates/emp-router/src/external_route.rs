//! Complete and streamed external request execution.

use super::*;

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
        let prepared = tools.prepare_or_borrow(body).map_err(tool_request_error)?;
        let body = prepared.as_ref();
        let provider = route.provider.value();
        let endpoint = endpoint(provider, route.protocol)?;
        let headers = upstream_headers(provider, route.protocol, incoming)?;
        let payload = project_prepared_external_payload(route, body)?;
        let encoded = request_encoding::encode_projected_request(&payload).map_err(|_| {
            RouterError::new(
                RouterErrorKind::InvalidRequest,
                422,
                FailureClass::RouterError,
                Some("request_serialization".to_owned()),
                None,
                "request serialization failed",
            )
        })?;
        drop(payload);
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
        let retry_after_seconds = retry_after::parse(response.header("retry-after"));
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
        let request_started = std::time::Instant::now();
        validate_stream_request(route, body)?;
        let mut tools = ExternalTools::default();
        let prepared = tools.prepare_or_borrow(body).map_err(tool_request_error)?;
        let body = prepared.as_ref();
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
                let mut payload = responses_to_chat(portable_body.as_ref(), &route.upstream_model)
                    .map_err(protocol_error)?;
                payload["stream"] = Value::Bool(true);
                payload["stream_options"] = serde_json::json!({"include_usage": true});
                let stream = ChatStream::new(&route.requested_model, ids.chat()?)
                    .with_custom_names(&custom_name_refs);
                (payload, StreamProjection::Chat(stream))
            }
            Protocol::AnthropicMessages => {
                let mut payload =
                    responses_to_anthropic(portable_body.as_ref(), &route.upstream_model)
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
                    project_portable_request(provider, portable_body.as_ref(), preserve_state)
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
        let encoded = request_encoding::encode_projected_request(&payload).map_err(|_| {
            RouterError::new(
                RouterErrorKind::InvalidRequest,
                422,
                FailureClass::RouterError,
                Some("request_serialization".to_owned()),
                None,
                "request serialization failed",
            )
        })?;
        drop(payload);
        let response = self
            .client
            .open(HttpMethod::Post, &endpoint, headers, Some(encoded), true)
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let retry_after_seconds = retry_after::parse(response.header("retry-after"));
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
            request_started,
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
