//! Event construction and bounded delta accumulation for Chat streams.

use super::*;

impl ChatStream {
    pub(super) fn event(&mut self, event: &'static str, mut value: Value) -> StreamEvent {
        self.sequence += 1;
        if let Some(object) = value.as_object_mut() {
            object.insert("sequence_number".to_owned(), Value::from(self.sequence));
        }
        StreamEvent { event, value }
    }

    pub(super) fn item_added(&mut self, output_index: usize, item: &Value) -> StreamEvent {
        self.event(
            "response.output_item.added",
            serde_json::json!({
                "type": "response.output_item.added",
                "output_index": output_index,
                "item": item
            }),
        )
    }

    pub(super) fn item_done(&mut self, output_index: usize, item: &Value) -> StreamEvent {
        self.event(
            "response.output_item.done",
            serde_json::json!({
                "type": "response.output_item.done",
                "output_index": output_index,
                "item": item
            }),
        )
    }

    pub(super) fn reasoning_event(
        &mut self,
        event: &'static str,
        item_id: &str,
        output_index: usize,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": item_id,
                "output_index": output_index,
                "content_index": 0,
                "delta": value
            }),
        )
    }

    pub(super) fn message_event(
        &mut self,
        event: &'static str,
        output_index: usize,
        content_index: usize,
        field: &str,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": content_index,
                field: value
            }),
        )
    }

    pub(super) fn tool_delta_event(
        &mut self,
        event: &'static str,
        output_index: usize,
        item_id: &str,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": item_id,
                "output_index": output_index,
                "delta": value
            }),
        )
    }

    pub(super) fn content_part(kind: ContentKind, value: &str) -> Value {
        let mut part = Map::new();
        part.insert(
            "type".to_owned(),
            Value::String(kind.event_name().to_owned()),
        );
        part.insert(
            kind.field_name().to_owned(),
            Value::String(value.to_owned()),
        );
        if kind == ContentKind::OutputText {
            part.insert("annotations".to_owned(), Value::Array(Vec::new()));
        }
        Value::Object(part)
    }

    pub(super) fn content_part_event(
        &mut self,
        event: &'static str,
        output_index: usize,
        content_index: usize,
        kind: ContentKind,
        value: &str,
    ) -> StreamEvent {
        self.event(
            event,
            serde_json::json!({
                "type": event,
                "item_id": self.message_id,
                "output_index": output_index,
                "content_index": content_index,
                "part": Self::content_part(kind, value)
            }),
        )
    }

    pub(super) fn message_output(&self) -> Value {
        let content = self
            .content_parts
            .iter()
            .map(|(kind, value)| Self::content_part(*kind, value))
            .collect::<Vec<_>>();
        serde_json::json!({
            "id": self.message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": content
        })
    }

    pub(super) fn consume_reasoning(
        &mut self,
        delta: &Map<String, Value>,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProtocolError> {
        let piece = chat_reasoning_text(delta);
        if piece.is_empty() {
            return Ok(());
        }
        self.add_stream_text(piece)?;
        self.saw_output = true;
        if self.message_started {
            self.late_reasoning.push_str(piece);
            return Ok(());
        }
        if !self.reasoning_started {
            self.reasoning_started = true;
            events.push(self.item_added(
                0,
                &serde_json::json!({
                    "id": self.reasoning_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                    "content": []
                }),
            ));
        }
        self.reasoning.push_str(piece);
        let reasoning_id = self.reasoning_id.clone();
        events.push(self.reasoning_event("response.reasoning_text.delta", &reasoning_id, 0, piece));
        Ok(())
    }

    pub(super) fn consume_content(
        &mut self,
        delta: &Map<String, Value>,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProtocolError> {
        let text = match delta.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(value @ Value::Array(_)) => combined_array_text(
                value,
                "Chat Completions upstream returned invalid message content",
            )?,
            Some(_) => {
                return Err(protocol_error(
                    "Chat Completions upstream returned invalid message content",
                ));
            }
        };
        let refusal = refusal_text(delta)?;
        for (kind, fragment) in [
            (ContentKind::OutputText, text),
            (ContentKind::Refusal, refusal),
        ] {
            if fragment.is_empty() {
                continue;
            }
            if self.message_closed {
                return Err(protocol_error(
                    "Chat Completions upstream returned content after a tool call",
                ));
            }
            self.add_stream_text(&fragment)?;
            self.saw_output = true;
            if !self.message_started {
                if self.reasoning_started {
                    events.push(
                        self.item_done(0, &reasoning_item(&self.reasoning_id, &self.reasoning)),
                    );
                }
                self.message_started = true;
                let message_index = usize::from(self.reasoning_started);
                events.push(self.item_added(
                    message_index,
                    &serde_json::json!({
                        "id": self.message_id,
                        "type": "message",
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    }),
                ));
            }
            let message_index = usize::from(self.reasoning_started);
            let content_index = self.content_index(kind);
            if content_index == self.content_parts.len() {
                self.content_parts.push((kind, fragment.clone()));
                events.push(self.content_part_event(
                    "response.content_part.added",
                    message_index,
                    content_index,
                    kind,
                    "",
                ));
            } else {
                self.content_parts[content_index].1.push_str(&fragment);
            }
            let event = match kind {
                ContentKind::OutputText => "response.output_text.delta",
                ContentKind::Refusal => "response.refusal.delta",
            };
            events.push(self.message_event(
                event,
                message_index,
                content_index,
                "delta",
                &fragment,
            ));
        }
        Ok(())
    }

    pub(super) fn content_index(&self, kind: ContentKind) -> usize {
        self.content_parts
            .iter()
            .position(|(existing, _)| *existing == kind)
            .unwrap_or(self.content_parts.len())
    }

    pub(super) fn consume_tool_calls(
        &mut self,
        delta: &Map<String, Value>,
    ) -> Result<(), ProtocolError> {
        let calls = match delta.get("tool_calls") {
            None => return Ok(()),
            Some(Value::Array(calls)) => calls,
            Some(_) => {
                return Err(protocol_error(
                    "Chat Completions upstream returned an invalid tool call",
                ));
            }
        };
        if calls.is_empty() {
            return Ok(());
        }
        self.saw_output = true;
        if self.message_started {
            self.message_closed = true;
        }
        for raw_call in calls {
            let Some(raw_call) = object(raw_call) else {
                return Err(protocol_error(
                    "Chat Completions upstream returned an invalid tool call",
                ));
            };
            let index = tool_index(raw_call.get("index"), self.tool_calls.len())?;
            let empty_function = Map::new();
            let function = match raw_call.get("function") {
                None | Some(Value::Null) => &empty_function,
                Some(Value::Object(function)) => function,
                Some(_) => {
                    return Err(protocol_error(
                        "Chat Completions upstream returned an invalid tool call",
                    ));
                }
            };
            let is_new = !self.tool_calls.contains_key(&index);
            let raw_id = string_at(raw_call, "id").unwrap_or_default();
            if is_new {
                if raw_id.is_empty() {
                    return Err(protocol_error(
                        "Chat Completions upstream returned an invalid tool call",
                    ));
                }
                if !self.tool_call_ids.insert(raw_id.to_owned()) {
                    return Err(protocol_error(
                        "Chat Completions upstream returned a duplicate tool call ID",
                    ));
                }
            }
            let state = self
                .tool_calls
                .entry(index)
                .or_insert_with(|| ToolCallState {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    extra_content: None,
                });
            if state.id.is_empty() {
                state.id = raw_id.to_owned();
            } else if !raw_id.is_empty() && raw_id != state.id {
                return Err(protocol_error(
                    "Chat Completions upstream returned a duplicate tool call ID",
                ));
            }
            if let Some(name) = function.get("name") {
                let Some(name) = name.as_str() else {
                    return Err(protocol_error(
                        "Chat Completions upstream returned an invalid tool call",
                    ));
                };
                state.name.push_str(name);
            }
            if let Some(extra) = raw_call
                .get("extra_content")
                .filter(|extra| extra.is_object())
            {
                state.extra_content = Some(extra.clone());
            }
            if let Some(arguments) = function.get("arguments") {
                let Some(arguments) = arguments.as_str() else {
                    return Err(protocol_error(
                        "Chat Completions upstream returned invalid tool arguments",
                    ));
                };
                state.arguments.push_str(arguments);
            }
        }
        Ok(())
    }

    pub(super) fn add_stream_text(&mut self, text: &str) -> Result<(), ProtocolError> {
        let total = self
            .stream_text_bytes
            .checked_add(text.len())
            .ok_or_else(|| upstream_error("upstream streamed text is too large"))?;
        if total > MAX_STREAM_TEXT_BYTES {
            return Err(upstream_error("upstream streamed text is too large"));
        }
        self.stream_text_bytes = total;
        Ok(())
    }
}
