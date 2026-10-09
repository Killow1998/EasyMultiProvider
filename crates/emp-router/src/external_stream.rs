//! Incremental external stream processing and Responses event synthesis.

use super::*;

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
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            if let Some(error) = self.failure.take() {
                return Err(error);
            }
            if self.finished {
                return Ok(None);
            }
            let result = if let Some(generated) = self.generated.as_mut() {
                match generated.next() {
                    Some(event) => self.consume_json(event, ChatFrame::Sse),
                    None => {
                        self.generated.take();
                        self.finish_projection()
                    }
                }
            } else {
                let next = self
                    .response
                    .as_mut()
                    .ok_or_else(|| invalid_request("stream response is no longer available"))?
                    .next_chunk()
                    .await
                    .map_err(transport_error);
                match next {
                    Ok(Some(chunk)) => self.consume_chunk(&chunk),
                    Ok(None) => self.consume_eof(),
                    Err(error) => Err(error),
                }
            };
            if let Err(error) = result {
                self.response.take();
                self.generated.take();
                self.finished = true;
                self.failure = Some(error);
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
        // Do not parse beyond a terminal even when the same network chunk has
        // trailing data. A malformed tail cannot invalidate a completed turn.
        for line in chunk.split_inclusive(|byte| *byte == b'\n') {
            let frames = self
                .parser
                .as_mut()
                .expect("active stream parser")
                .push_frames(line)
                .map_err(sse_transport_error)?;
            if !frames.is_empty() {
                self.saw_sse = true;
                self.raw_body.clear();
            }
            self.consume_frames(frames)?;
            if self.finished {
                break;
            }
        }
        Ok(())
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
        self.finished = self.generated.is_none();
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
        self.observation
            .observe(&value, matches!(frame, ChatFrame::Sse));
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
        if self.pending.back().is_some_and(|event| {
            matches!(
                event.event.as_str(),
                "response.completed" | "response.incomplete" | "response.failed" | "error"
            )
        }) {
            self.finished = true;
            self.response.take();
            self.generated.take();
        }
        Ok(())
    }

    fn consume_ordinary_body(&mut self) -> Result<(), RouterError> {
        let value: Value =
            serde_json::from_slice(&std::mem::take(&mut self.raw_body)).map_err(|_| {
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
                self.generated = Some(response_json_stream_events(value, &self.ids, true)?);
                Ok(())
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
