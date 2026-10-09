//! Native Responses SSE framing and stream state.

use super::*;

fn may_carry_context_error(event: &Value) -> bool {
    // OpenAI-compatible gateways also stream bare `{"error":{...}}` events
    // without a `type`; ordinary content events never carry an error object.
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("error" | "response.failed" | "response.incomplete")
    ) || event.get("error").is_some_and(|error| !error.is_null())
}

impl NativeStream {
    pub async fn next_event(&mut self) -> Result<Option<NativeStreamEvent>, RouterError> {
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
                    Some(event) => self.push_event(event, None),
                    None => {
                        self.generated.take();
                        self.finished = true;
                        Ok(())
                    }
                }
            } else {
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

    fn consume_chunk(&mut self, chunk: &[u8]) -> Result<(), RouterError> {
        // Keep bytes only while a non-SSE response might be ordinary JSON.
        // Delivered SSE frames and comment heartbeats do not occupy this buffer.
        if !self.saw_data && !self.declared_sse {
            if chunk.len() > MAX_UPSTREAM_BODY_BYTES.saturating_sub(self.raw_body.len()) {
                return Err(crate::upstream_body_too_large());
            }
            self.raw_body.extend_from_slice(chunk);
        }
        self.line_buffer.extend_from_slice(chunk);
        // Scan each byte once: lines are consumed through a moving offset and
        // the buffer is compacted a single time per chunk. Bytes left over
        // from previous chunks are already known to contain no newline.
        let buffer = std::mem::take(&mut self.line_buffer);
        let mut start = 0;
        let mut scan = self.line_scanned.min(buffer.len());
        while let Some(relative) = buffer[scan..].iter().position(|byte| *byte == b'\n') {
            let end = scan + relative;
            let wire = &buffer[start..=end];
            self.consume_line(&wire[..wire.len() - 1], wire)?;
            if self.saw_data {
                self.raw_body = Vec::new();
            }
            if self.finished {
                return Ok(());
            }
            start = end + 1;
            scan = start;
        }
        self.line_buffer = buffer;
        self.line_buffer.drain(..start);
        self.line_scanned = self.line_buffer.len();
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
            self.line_scanned = 0;
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
            self.generated = Some(response_json_stream_events(value, &self.ids, false)?);
            self.raw_body = Vec::new();
            self.pending_wire = Vec::new();
            self.line_buffer = Vec::new();
        }
        if !self.saw_terminal && self.generated.is_none() {
            return Err(native_stream_error(
                502,
                FailureClass::StreamIncomplete,
                Some("stream_incomplete"),
                "upstream Responses stream ended before response.completed",
            ));
        }
        self.response.take();
        self.finished = self.generated.is_none();
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
        if may_carry_context_error(&event)
            && is_explicit_context_error(400, "application/json", &data)
        {
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
        self.observation.observe(&event, original_frame.is_some());
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
            self.finished = true;
            self.response.take();
            self.generated.take();
        }
        let frame = if !model_changed && !self.plaintext_collaboration {
            match original_frame {
                Some(frame) => frame,
                None => native_sse_frame(&event_type, &projected)?,
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn detached_stream() -> NativeStream {
        NativeStream {
            request_started: std::time::Instant::now(),
            observation: Default::default(),
            usage_owner: None,
            response: None,
            requested_model: "gpt-test".to_owned(),
            upstream_model: "gpt-test".to_owned(),
            plaintext_collaboration: false,
            declared_sse: true,
            line_buffer: Vec::new(),
            line_scanned: 0,
            pending_wire: Vec::new(),
            pending_data: Vec::new(),
            pending: VecDeque::new(),
            generated: None,
            raw_body: Vec::new(),
            saw_data: false,
            saw_terminal: false,
            finished: false,
            failure: None,
            ids: ProjectionIds::new("resp_test", "msg_test", "rs_test", "rs_late_test"),
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn sse_lines_split_across_every_byte_keep_frames_and_scan_offset() {
        let wire = b": keepalive\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"hi\"}\r\n\r\ndata: {\"type\":\"response.in_progress\"}\n\n";
        let mut whole = detached_stream();
        whole.consume_chunk(wire).unwrap();
        let mut split = detached_stream();
        for byte in wire {
            split.consume_chunk(std::slice::from_ref(byte)).unwrap();
            assert!(split.line_scanned <= split.line_buffer.len());
            assert!(!split.line_buffer.contains(&b'\n'));
        }
        let frames = |stream: &NativeStream| {
            stream
                .pending
                .iter()
                .map(|event| (event.event.clone(), event.frame.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(frames(&whole), frames(&split));
        assert_eq!(whole.pending.len(), 2);
        assert_eq!(
            whole.pending[0].frame,
            b"data: {\"type\":\"response.output_text.delta\",\r\ndata: \"delta\":\"hi\"}\r\n\r\n"
        );
        assert!(split.line_buffer.is_empty());
        assert_eq!(split.line_scanned, 0);

        let mut partial = detached_stream();
        partial.consume_chunk(b"data: {\"type\"").unwrap();
        assert_eq!(partial.line_scanned, partial.line_buffer.len());
        partial
            .consume_chunk(b":\"response.in_progress\"}\n\n")
            .unwrap();
        assert_eq!(partial.pending.len(), 1);
        assert_eq!(partial.line_scanned, 0);
    }

    #[test]
    fn context_errors_are_observed_only_on_error_events() {
        let ordinary = json!({
            "type": "response.output_text.delta",
            "delta": "context_length_exceeded",
        });
        let mut stream = detached_stream();
        let frame = format!("data: {ordinary}\n\n");
        stream.consume_chunk(frame.as_bytes()).unwrap();
        assert_eq!(stream.pending.len(), 1);
        assert!(!stream.saw_terminal);

        let failed = json!({
            "type": "response.failed",
            "response": {"error": {"code": "context_length_exceeded"}},
        });
        let frame = format!("data: {failed}\n\n");
        let error = stream
            .consume_chunk(frame.as_bytes())
            .expect_err("terminal context error is classified");
        assert_eq!(error.status(), 413);

        let untyped = json!({"error": {"code": "context_length_exceeded"}});
        let frame = format!("data: {untyped}\n\n");
        let error = detached_stream()
            .consume_chunk(frame.as_bytes())
            .expect_err("untyped gateway context error is classified");
        assert_eq!(error.status(), 413);
    }

    #[test]
    fn context_error_observation_cost_has_body_and_node_caps() {
        // Evidence beyond the body cap is not inspected: the event passes
        // through as an ordinary terminal instead of being classified.
        let mut stream = detached_stream();
        let failed = json!({
            "type": "response.failed",
            "response": {
                "output": "x".repeat(80 * 1024),
                "error": {"code": "context_length_exceeded"},
            },
        });
        let frame = format!("data: {failed}\n\n");
        stream.consume_chunk(frame.as_bytes()).unwrap();
        assert_eq!(stream.pending.len(), 1);
        assert!(stream.saw_terminal);

        let mut stream = detached_stream();
        let failed = json!({
            "type": "response.failed",
            "response": {
                "details": vec![Value::Null; 5_000],
                "error": {"code": "context_length_exceeded"},
            },
        });
        let frame = format!("data: {failed}\n\n");
        stream.consume_chunk(frame.as_bytes()).unwrap();
        assert!(stream.saw_terminal);
    }
}
