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
