//! Native Responses SSE framing and stream state.

use super::*;

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
