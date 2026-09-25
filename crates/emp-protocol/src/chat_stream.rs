//! Stateful Chat Completions stream projection.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatFrame {
    Sse,
    Ordinary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub event: &'static str,
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    OutputText,
    Refusal,
}

impl ContentKind {
    fn event_name(self) -> &'static str {
        match self {
            Self::OutputText => "output_text",
            Self::Refusal => "refusal",
        }
    }

    fn field_name(self) -> &'static str {
        match self {
            Self::OutputText => "text",
            Self::Refusal => "refusal",
        }
    }
}

#[derive(Debug, Clone)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
    extra_content: Option<Value>,
}

/// Bounded state machine for Chat Completions chunks, SSE deltas and
/// gateways that ignore `stream: true` and return one ordinary response.
#[derive(Clone)]
pub struct ChatStream {
    response_id: String,
    message_id: String,
    reasoning_id: String,
    late_reasoning_id: String,
    sequence: u64,
    response: Map<String, Value>,
    reasoning: String,
    late_reasoning: String,
    content_parts: Vec<(ContentKind, String)>,
    tool_calls: BTreeMap<i64, ToolCallState>,
    tool_call_ids: BTreeSet<String>,
    custom_names: BTreeSet<String>,
    usage: Option<Value>,
    finish_reason: Option<Value>,
    saw_output: bool,
    saw_done: bool,
    started: bool,
    finished: bool,
    ordinary_complete: bool,
    message_started: bool,
    message_closed: bool,
    reasoning_started: bool,
    stream_text_bytes: usize,
    unknown_fields: Vec<UnknownField>,
}

impl fmt::Debug for ChatStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatStream")
            .field("response_id", &self.response_id)
            .field("message_started", &self.message_started)
            .field("message_closed", &self.message_closed)
            .field("reasoning_started", &self.reasoning_started)
            .field("saw_done", &self.saw_done)
            .field("unknown_fields", &self.unknown_fields.len())
            .finish()
    }
}

impl ChatStream {
    pub fn new(response_model: impl Into<String>, ids: ChatIds) -> Self {
        let response_model = response_model.into();
        let response = serde_json::json!({
            "id": ids.response.clone(),
            "object": "response",
            "status": "in_progress",
            "model": response_model,
            "output": []
        });
        Self {
            response_id: ids.response,
            message_id: ids.message,
            reasoning_id: ids.reasoning,
            late_reasoning_id: ids.late_reasoning,
            sequence: 0,
            response: response.as_object().expect("literal response").clone(),
            reasoning: String::new(),
            late_reasoning: String::new(),
            content_parts: Vec::new(),
            tool_calls: BTreeMap::new(),
            tool_call_ids: BTreeSet::new(),
            custom_names: BTreeSet::new(),
            usage: None,
            finish_reason: None,
            saw_output: false,
            saw_done: false,
            started: false,
            finished: false,
            ordinary_complete: false,
            message_started: false,
            message_closed: false,
            reasoning_started: false,
            stream_text_bytes: 0,
            unknown_fields: Vec::new(),
        }
    }

    pub fn with_custom_names(mut self, custom_names: &[&str]) -> Self {
        self.custom_names = custom_names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    pub fn start_event(&mut self) -> Result<StreamEvent, ProtocolError> {
        if self.started || self.finished {
            return Err(protocol_error("Chat Completions stream already started"));
        }
        self.started = true;
        let response = Value::Object(self.response.clone());
        Ok(self.event(
            "response.created",
            serde_json::json!({"type": "response.created", "response": response}),
        ))
    }

    pub fn unknown_fields(&self) -> &[UnknownField] {
        &self.unknown_fields
    }

    pub fn mark_done(&mut self) {
        self.saw_done = true;
    }

    pub fn ordinary_complete(&self) -> bool {
        self.ordinary_complete
    }

    pub fn push(
        &mut self,
        chunk: &Value,
        frame: ChatFrame,
    ) -> Result<Vec<StreamEvent>, ProtocolError> {
        if !self.started || self.finished || self.saw_done {
            return Err(protocol_error(
                "Chat Completions stream is not accepting chunks",
            ));
        }
        let mut events = Vec::new();
        collect_unknown_fields(&mut self.unknown_fields, chunk);
        let Some(root) = object(chunk) else {
            return Err(protocol_error(
                "Chat Completions upstream returned malformed SSE data",
            ));
        };
        if root.get("error").is_some_and(json_truthy) {
            return Err(upstream_error(
                "Chat Completions upstream returned an error",
            ));
        }
        if let Some(usage) = root.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(projected_usage(usage)?);
        }
        if let Some(service_tier) = string_at(root, "service_tier") {
            self.response.insert(
                "service_tier".to_owned(),
                Value::String(service_tier.to_owned()),
            );
        }
        let choice = root
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .cloned();
        let Some(choice) = choice else {
            if self.usage.is_some() {
                return Ok(events);
            }
            return Err(protocol_error(
                "Chat Completions upstream returned an invalid stream chunk",
            ));
        };
        let choice = object(&choice).ok_or_else(|| {
            protocol_error("Chat Completions upstream returned an invalid stream chunk")
        })?;
        if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
            self.finish_reason = Some(reason.clone());
        }
        let mut delta = choice
            .get("delta")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        if object(&delta).is_none_or(Map::is_empty)
            && let Some(message) = choice.get("message").filter(|value| value.is_object())
        {
            delta = message.clone();
            self.ordinary_complete = frame == ChatFrame::Ordinary
                && choice
                    .get("finish_reason")
                    .is_some_and(|value| !value.is_null());
        }
        let delta = object(&delta).ok_or_else(|| {
            protocol_error("Chat Completions upstream returned an invalid stream chunk")
        })?;
        self.consume_reasoning(delta, &mut events)?;
        self.consume_content(delta, &mut events)?;
        self.consume_tool_calls(delta)?;
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Vec<StreamEvent>, ProtocolError> {
        if !self.started || self.finished {
            return Err(protocol_error(
                "Chat Completions stream cannot finish twice",
            ));
        }
        let mut events = Vec::new();
        if !self.saw_done && !self.ordinary_complete {
            return Err(stream_error(
                "Chat Completions upstream ended before [DONE]",
            ));
        }
        if !self.saw_output {
            return Err(stream_error(
                "upstream returned an empty Chat Completions response",
            ));
        }
        let reason = incomplete_reason(self.finish_reason.as_ref())?;
        self.finished = true;
        let message_index = usize::from(self.reasoning_started);
        if self.reasoning_started && !self.message_started {
            events.push(self.item_done(0, &reasoning_item(&self.reasoning_id, &self.reasoning)));
        }
        if self.message_started {
            let parts = self.content_parts.clone();
            for (content_index, (kind, value)) in parts.iter().enumerate() {
                let event = match kind {
                    ContentKind::OutputText => "response.output_text.done",
                    ContentKind::Refusal => "response.refusal.done",
                };
                events.push(self.message_event(
                    event,
                    message_index,
                    content_index,
                    kind.field_name(),
                    value,
                ));
                events.push(self.content_part_event(
                    "response.content_part.done",
                    message_index,
                    content_index,
                    *kind,
                    value,
                ));
            }
            events.push(self.item_done(message_index, &self.message_output()));
        }
        let late_index = message_index + usize::from(self.message_started);
        if !self.late_reasoning.is_empty() {
            let text = self.late_reasoning.clone();
            events.push(self.item_added(
                late_index,
                &serde_json::json!({
                    "id": self.late_reasoning_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": [],
                    "content": []
                }),
            ));
            let late_reasoning_id = self.late_reasoning_id.clone();
            events.push(self.reasoning_event(
                "response.reasoning_text.delta",
                &late_reasoning_id,
                late_index,
                &text,
            ));
            events
                .push(self.item_done(late_index, &reasoning_item(&self.late_reasoning_id, &text)));
        }
        let tool_base = late_index + usize::from(!self.late_reasoning.is_empty());
        let mut function_outputs = Vec::new();
        for (position, (_, state)) in std::mem::take(&mut self.tool_calls).into_iter().enumerate() {
            if state.name.is_empty() {
                return Err(upstream_error(
                    "upstream returned a tool call without a name",
                ));
            }
            let arguments = tool_arguments(Some(&Value::String(state.arguments.clone())))?;
            let custom = self.custom_names.contains(&state.name);
            let item_id = if custom {
                custom_tool_id(&state.id)
            } else {
                state.id.clone()
            };
            let output_index = tool_base + position;
            let call_type = if custom {
                "custom_tool_call"
            } else {
                "function_call"
            };
            let mut added = serde_json::json!({
                "id": item_id,
                "type": call_type,
                "status": "in_progress",
                "call_id": state.id,
                "name": state.name,
            });
            if custom {
                added["input"] = Value::String(String::new());
            } else {
                added["arguments"] = Value::String(String::new());
            }
            if let Some(extra) = &state.extra_content {
                added["extra_content"] = extra.clone();
            }
            events.push(self.item_added(output_index, &added));
            let projected_input = if custom {
                custom_tool_input(&arguments)
            } else {
                arguments.clone()
            };
            let mut output = serde_json::json!({
                "id": item_id,
                "type": call_type,
                "status": "completed",
                "call_id": state.id,
                "name": state.name,
            });
            if custom {
                output["input"] = Value::String(projected_input.clone());
            } else {
                output["arguments"] = Value::String(arguments.clone());
            }
            if let Some(extra) = state.extra_content {
                output["extra_content"] = extra;
            }
            if custom {
                events.push(self.tool_delta_event(
                    "response.custom_tool_call_input.delta",
                    output_index,
                    &item_id,
                    &projected_input,
                ));
            } else {
                events.push(self.tool_delta_event(
                    "response.function_call_arguments.delta",
                    output_index,
                    &item_id,
                    &arguments,
                ));
                events.push(self.event(
                    "response.function_call_arguments.done",
                    serde_json::json!({
                        "type": "response.function_call_arguments.done",
                        "item_id": item_id,
                        "output_index": output_index,
                        "arguments": arguments,
                    }),
                ));
            }
            function_outputs.push(output.clone());
            events.push(self.item_done(output_index, &output));
        }
        let mut outputs = Vec::new();
        if self.reasoning_started {
            outputs.push(reasoning_item(&self.reasoning_id, &self.reasoning));
        }
        if self.message_started {
            outputs.push(self.message_output());
        }
        if !self.late_reasoning.is_empty() {
            outputs.push(reasoning_item(
                &self.late_reasoning_id,
                &self.late_reasoning,
            ));
        }
        outputs.extend(function_outputs);
        self.response
            .insert("output".to_owned(), Value::Array(outputs));
        let output_text = self
            .content_parts
            .iter()
            .find(|(kind, _)| *kind == ContentKind::OutputText)
            .map_or_else(String::new, |(_, value)| value.clone());
        self.response
            .insert("output_text".to_owned(), Value::String(output_text));
        self.response.insert(
            "status".to_owned(),
            Value::String(
                if reason.is_some() {
                    "incomplete"
                } else {
                    "completed"
                }
                .to_owned(),
            ),
        );
        if let Some(reason) = reason {
            self.response.insert(
                "incomplete_details".to_owned(),
                serde_json::json!({"reason": reason}),
            );
        }
        if let Some(usage) = self.usage.clone() {
            self.response.insert("usage".to_owned(), usage);
        }
        let event = if reason.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let response = Value::Object(self.response.clone());
        events.push(self.event(
            event,
            serde_json::json!({"type": event, "response": response}),
        ));
        Ok(events)
    }
}

mod events;
