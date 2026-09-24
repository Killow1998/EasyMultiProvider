//! Anthropic SSE stream state and event synthesis.

use super::*;
use super::response::{anthropic_usage, custom_tool_id, custom_tool_input, incomplete_reason, output_message, tool_arguments};

#[derive(Debug, Clone)]
pub struct AnthropicIds {
    pub(super) response: String,
    message: String,
    message_offset: usize,
}

impl AnthropicIds {
    pub fn new(response: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            message: message.into(),
            message_offset: 0,
        }
    }

    pub(super) fn next_message(&mut self) -> String {
        let result = if self.message_offset == 0 {
            self.message.clone()
        } else {
            format!("{}_{}", self.message, self.message_offset)
        };
        self.message_offset += 1;
        result
    }
}

const MAX_ANTHROPIC_STREAM_TEXT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ANTHROPIC_TOOL_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
enum AnthropicBlockKind {
    Text {
        id: String,
        parts: Vec<String>,
        explicit: bool,
    },
    Tool {
        id: String,
        call_id: String,
        name: String,
        custom: bool,
        initial_input: Map<String, Value>,
        json_parts: Vec<String>,
        json_bytes: usize,
    },
    SuppressedReasoning,
}

#[derive(Debug, Clone)]
struct AnthropicBlockState {
    output_index: Option<usize>,
    closed: bool,
    item: Option<Value>,
    kind: AnthropicBlockKind,
}

/// Pure Anthropic Messages stream state machine.
///
/// The transport supplies already parsed JSON events. This type owns event
/// ordering, block validation, usage accumulation and terminal truth.
#[derive(Clone)]
pub struct AnthropicStream {
    ids: AnthropicIds,
    sequence: u64,
    response: Map<String, Value>,
    blocks: BTreeMap<usize, AnthropicBlockState>,
    ordered_blocks: Vec<usize>,
    tool_call_ids: BTreeSet<String>,
    custom_names: BTreeSet<String>,
    all_text: String,
    text_bytes: usize,
    stop_reason: Option<Value>,
    usage: Map<String, Value>,
    saw_message_stop: bool,
    started: bool,
    finished: bool,
}

impl std::fmt::Debug for AnthropicStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicStream")
            .field("response_id", &self.ids.response)
            .field("blocks", &self.blocks.len())
            .field("saw_message_stop", &self.saw_message_stop)
            .field("started", &self.started)
            .field("finished", &self.finished)
            .finish()
    }
}

impl AnthropicStream {
    pub fn new(
        response_model: impl Into<String>,
        ids: AnthropicIds,
        custom_names: &[&str],
    ) -> Self {
        let response = json!({
            "id": ids.response.clone(),
            "object": "response",
            "status": "in_progress",
            "model": response_model.into(),
            "output": []
        });
        Self {
            ids,
            sequence: 0,
            response: response.as_object().expect("literal response").clone(),
            blocks: BTreeMap::new(),
            ordered_blocks: Vec::new(),
            tool_call_ids: BTreeSet::new(),
            custom_names: custom_names.iter().map(|name| (*name).to_owned()).collect(),
            all_text: String::new(),
            text_bytes: 0,
            stop_reason: None,
            usage: Map::new(),
            saw_message_stop: false,
            started: false,
            finished: false,
        }
    }

    pub fn start_event(&mut self) -> Result<crate::StreamEvent, AnthropicError> {
        if self.started || self.finished {
            return Err(upstream_error("Anthropic stream already started"));
        }
        self.started = true;
        let response = Value::Object(self.response.clone());
        Ok(self.event(
            "response.created",
            json!({"type": "response.created", "response": response}),
        ))
    }

    pub fn push(&mut self, event: &Value) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        if !self.started || self.finished {
            return Err(upstream_error("Anthropic stream is not accepting events"));
        }
        let event = object(event)
            .ok_or_else(|| upstream_error("Anthropic upstream returned an invalid stream event"))?;
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "error" {
            return Err(upstream_error("Anthropic upstream returned an error event"));
        }
        match event_type {
            "message_start" => {
                if let Some(value) = event
                    .get("message")
                    .and_then(object)
                    .and_then(|message| message.get("usage"))
                    .and_then(object)
                {
                    self.usage.extend(value.clone());
                }
                Ok(Vec::new())
            }
            "message_delta" => {
                if let Some(value) = event.get("usage").and_then(object) {
                    self.usage.extend(value.clone());
                }
                if let Some(reason) = event
                    .get("delta")
                    .and_then(object)
                    .and_then(|delta| delta.get("stop_reason"))
                    .filter(|reason| python_truthy(Some(reason)))
                {
                    self.stop_reason = Some(reason.clone());
                }
                Ok(Vec::new())
            }
            "message_stop" => {
                self.saw_message_stop = true;
                Ok(Vec::new())
            }
            "content_block_start" => self.start_block(event),
            "content_block_delta" => self.push_delta(event),
            "content_block_stop" => self.stop_block(event),
            _ => Ok(Vec::new()),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        if !self.started || self.finished {
            return Err(stream_error("Anthropic stream cannot finish twice"));
        }
        if !self.saw_message_stop {
            return Err(stream_error("Anthropic upstream ended before message_stop"));
        }
        let incomplete = incomplete_reason(self.stop_reason.as_ref())?;
        let mut events = Vec::new();
        for raw_index in self.ordered_blocks.clone() {
            let (explicit, closed) = match &self.blocks[&raw_index].kind {
                AnthropicBlockKind::Text { explicit, .. } => {
                    (*explicit, self.blocks[&raw_index].closed)
                }
                _ => (true, self.blocks[&raw_index].closed),
            };
            if closed {
                continue;
            }
            if explicit {
                return Err(stream_error(
                    "Anthropic upstream ended with an unfinished content block",
                ));
            }
            events.extend(self.close_text(raw_index)?);
        }
        let output = self
            .ordered_blocks
            .iter()
            .filter_map(|index| self.blocks[index].item.clone())
            .collect::<Vec<_>>();
        if output.is_empty() {
            return Err(stream_error(
                "upstream returned an empty Anthropic Messages response",
            ));
        }
        self.finished = true;
        self.response.insert(
            "status".to_owned(),
            Value::String(
                if incomplete.is_some() {
                    "incomplete"
                } else {
                    "completed"
                }
                .to_owned(),
            ),
        );
        self.response
            .insert("output".to_owned(), Value::Array(output));
        self.response.insert(
            "output_text".to_owned(),
            Value::String(self.all_text.clone()),
        );
        if !self.usage.is_empty() {
            self.response.insert(
                "usage".to_owned(),
                anthropic_usage(&Value::Object(self.usage.clone())),
            );
        }
        if let Some(reason) = incomplete {
            self.response
                .insert("incomplete_details".to_owned(), json!({"reason": reason}));
        }
        let terminal = if incomplete.is_some() {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let response = Value::Object(self.response.clone());
        events.push(self.event(terminal, json!({"type": terminal, "response": response})));
        Ok(events)
    }

    fn raw_index(event: &Map<String, Value>, default: usize) -> Result<usize, AnthropicError> {
        match event.get("index") {
            None => Ok(default),
            Some(Value::Number(number)) => number
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    upstream_error("Anthropic upstream returned an invalid content index")
                }),
            Some(_) => Err(upstream_error(
                "Anthropic upstream returned an invalid content index",
            )),
        }
    }

    fn start_block(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, self.blocks.len())?;
        if self.blocks.contains_key(&raw_index) {
            return Err(upstream_error(
                "Anthropic upstream repeated a content block",
            ));
        }
        let block = event.get("content_block").and_then(object).ok_or_else(|| {
            upstream_error("Anthropic upstream returned an invalid content block")
        })?;
        let output_index = self.ordered_blocks.len();
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let initial = match block.get("text") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned invalid message content",
                        ));
                    }
                };
                let id = self.ids.next_message();
                self.blocks.insert(
                    raw_index,
                    AnthropicBlockState {
                        output_index: Some(output_index),
                        closed: false,
                        item: None,
                        kind: AnthropicBlockKind::Text {
                            id: id.clone(),
                            parts: Vec::new(),
                            explicit: true,
                        },
                    },
                );
                self.ordered_blocks.push(raw_index);
                let mut events = vec![
                    self.event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added", "output_index": output_index,
                            "item": {"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}
                        }),
                    ),
                    self.event(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added", "item_id": id,
                            "output_index": output_index, "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        }),
                    ),
                ];
                if let Some(initial) = initial.filter(|text| !text.is_empty()) {
                    events.extend(self.push_text(raw_index, &initial)?);
                }
                Ok(events)
            }
            "tool_use" => {
                let raw_id = required_upstream_string(
                    block.get("id"),
                    "Anthropic upstream returned an invalid tool call",
                )?;
                let name = required_upstream_string(
                    block.get("name"),
                    "Anthropic upstream returned an invalid tool call",
                )?;
                let initial_input = match block.get("input") {
                    None => Map::new(),
                    Some(Value::Object(input)) => input.clone(),
                    Some(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned an invalid tool call",
                        ));
                    }
                };
                if !self.tool_call_ids.insert(raw_id.to_owned()) {
                    return Err(upstream_error(
                        "Anthropic upstream returned a duplicate tool call ID",
                    ));
                }
                let custom = self.custom_names.contains(name);
                let id = if custom {
                    custom_tool_id(raw_id)
                } else {
                    raw_id.to_owned()
                };
                self.blocks.insert(
                    raw_index,
                    AnthropicBlockState {
                        output_index: Some(output_index),
                        closed: false,
                        item: None,
                        kind: AnthropicBlockKind::Tool {
                            id: id.clone(),
                            call_id: raw_id.to_owned(),
                            name: name.to_owned(),
                            custom,
                            initial_input,
                            json_parts: Vec::new(),
                            json_bytes: 0,
                        },
                    },
                );
                self.ordered_blocks.push(raw_index);
                let mut item = json!({
                    "id": id, "type": if custom { "custom_tool_call" } else { "function_call" },
                    "status": "in_progress", "call_id": raw_id, "name": name
                });
                item[if custom { "input" } else { "arguments" }] = Value::String(String::new());
                Ok(vec![self.event(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "output_index": output_index, "item": item}),
                )])
            }
            "thinking" | "redacted_thinking" => {
                self.blocks.insert(
                    raw_index,
                    AnthropicBlockState {
                        output_index: None,
                        closed: false,
                        item: None,
                        kind: AnthropicBlockKind::SuppressedReasoning,
                    },
                );
                Ok(Vec::new())
            }
            _ => Err(upstream_error(
                "Anthropic upstream returned an unsupported content block",
            )),
        }
    }

    fn push_delta(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, 0)?;
        let delta = event.get("delta").and_then(object).ok_or_else(|| {
            upstream_error("Anthropic upstream returned an invalid content delta")
        })?;
        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
        if !self.blocks.contains_key(&raw_index) && delta_type == "text_delta" {
            let output_index = self.ordered_blocks.len();
            let id = self.ids.next_message();
            self.blocks.insert(
                raw_index,
                AnthropicBlockState {
                    output_index: Some(output_index),
                    closed: false,
                    item: None,
                    kind: AnthropicBlockKind::Text {
                        id: id.clone(),
                        parts: Vec::new(),
                        explicit: false,
                    },
                },
            );
            self.ordered_blocks.push(raw_index);
            let mut events = vec![
                self.event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added", "output_index": output_index,
                        "item": {"id": id, "type": "message", "status": "in_progress", "role": "assistant", "content": []}
                    }),
                ),
                self.event(
                    "response.content_part.added",
                    json!({
                        "type": "response.content_part.added", "item_id": id,
                        "output_index": output_index, "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []}
                    }),
                ),
            ];
            if let Some(piece) = delta.get("text").and_then(Value::as_str) {
                if !piece.is_empty() {
                    events.extend(self.push_text(raw_index, piece)?);
                }
                return Ok(events);
            }
            return Err(upstream_error(
                "Anthropic upstream returned invalid message content",
            ));
        }
        let state = self.blocks.get(&raw_index).ok_or_else(|| {
            upstream_error("Anthropic upstream returned a delta for an unknown content block")
        })?;
        if state.closed {
            return Err(upstream_error(
                "Anthropic upstream returned a delta for an unknown content block",
            ));
        }
        if matches!(state.kind, AnthropicBlockKind::SuppressedReasoning) {
            return Ok(Vec::new());
        }
        match (&state.kind, delta_type) {
            (AnthropicBlockKind::Text { .. }, "text_delta") => {
                let piece = delta.get("text").and_then(Value::as_str).ok_or_else(|| {
                    upstream_error("Anthropic upstream returned invalid message content")
                })?;
                self.push_text(raw_index, piece)
            }
            (AnthropicBlockKind::Tool { .. }, "input_json_delta") => {
                let piece = delta
                    .get("partial_json")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        upstream_error("Anthropic upstream returned invalid tool input")
                    })?;
                let bytes = piece.len();
                let (custom, item_id, output_index) = {
                    let state = self.blocks.get_mut(&raw_index).expect("checked block");
                    let AnthropicBlockKind::Tool {
                        custom,
                        id,
                        json_parts,
                        json_bytes,
                        ..
                    } = &mut state.kind
                    else {
                        unreachable!()
                    };
                    if json_bytes.saturating_add(bytes) > MAX_ANTHROPIC_TOOL_INPUT_BYTES {
                        return Err(upstream_error("Anthropic upstream tool input is too large"));
                    }
                    *json_bytes += bytes;
                    json_parts.push(piece.to_owned());
                    (
                        *custom,
                        id.clone(),
                        state.output_index.expect("tool output index"),
                    )
                };
                if piece.is_empty() || custom {
                    Ok(Vec::new())
                } else {
                    Ok(vec![self.event(
                        "response.function_call_arguments.delta",
                        json!({
                            "type": "response.function_call_arguments.delta", "item_id": item_id,
                            "output_index": output_index, "delta": piece
                        }),
                    )])
                }
            }
            _ => Err(upstream_error(
                "Anthropic upstream returned a mismatched content delta",
            )),
        }
    }

    fn push_text(
        &mut self,
        raw_index: usize,
        piece: &str,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        if piece.is_empty() {
            return Ok(Vec::new());
        }
        let bytes = piece.len();
        if self.text_bytes.saturating_add(bytes) > MAX_ANTHROPIC_STREAM_TEXT_BYTES {
            return Err(upstream_error("upstream streamed text is too large"));
        }
        self.text_bytes += bytes;
        self.all_text.push_str(piece);
        let (id, output_index) = {
            let state = self.blocks.get_mut(&raw_index).expect("text block exists");
            let AnthropicBlockKind::Text { id, parts, .. } = &mut state.kind else {
                unreachable!()
            };
            parts.push(piece.to_owned());
            (id.clone(), state.output_index.expect("text output index"))
        };
        Ok(vec![self.event(
            "response.output_text.delta",
            json!({
                "type": "response.output_text.delta", "item_id": id,
                "output_index": output_index, "content_index": 0, "delta": piece
            }),
        )])
    }

    fn stop_block(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, 0)?;
        let state = self
            .blocks
            .get(&raw_index)
            .ok_or_else(|| upstream_error("Anthropic upstream stopped an unknown content block"))?;
        if state.closed {
            return Err(upstream_error(
                "Anthropic upstream stopped an unknown content block",
            ));
        }
        if matches!(state.kind, AnthropicBlockKind::SuppressedReasoning) {
            self.blocks
                .get_mut(&raw_index)
                .expect("checked block")
                .closed = true;
            return Ok(Vec::new());
        }
        if matches!(state.kind, AnthropicBlockKind::Text { .. }) {
            return self.close_text(raw_index);
        }
        self.close_tool(raw_index)
    }

    fn close_text(&mut self, raw_index: usize) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let (id, output_index, text) = {
            let state = self.blocks.get_mut(&raw_index).expect("text block exists");
            let AnthropicBlockKind::Text { id, parts, .. } = &state.kind else {
                unreachable!()
            };
            let id = id.clone();
            let text = parts.concat();
            let output_index = state.output_index.expect("text output index");
            let item = output_message(&id, &text);
            state.item = Some(item);
            state.closed = true;
            (id, output_index, text)
        };
        let item = self.blocks[&raw_index].item.clone().expect("text item");
        Ok(vec![
            self.event(
                "response.output_text.done",
                json!({
                    "type": "response.output_text.done", "item_id": id,
                    "output_index": output_index, "content_index": 0, "text": text
                }),
            ),
            self.event(
                "response.content_part.done",
                json!({
                    "type": "response.content_part.done", "item_id": id,
                    "output_index": output_index, "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []}
                }),
            ),
            self.event(
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": output_index, "item": item}),
            ),
        ])
    }

    fn close_tool(&mut self, raw_index: usize) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let (id, call_id, name, custom, output_index, tool_input) = {
            let state = self.blocks.get(&raw_index).expect("tool block exists");
            let AnthropicBlockKind::Tool {
                id,
                call_id,
                name,
                custom,
                initial_input,
                json_parts,
                ..
            } = &state.kind
            else {
                unreachable!()
            };
            let input = if json_parts.is_empty() {
                Value::Object(initial_input.clone())
            } else {
                match serde_json::from_str::<Value>(&json_parts.concat()) {
                    Ok(Value::Object(input)) => Value::Object(input),
                    Ok(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned invalid tool input",
                        ));
                    }
                    Err(_) => {
                        return Err(upstream_error(
                            "Anthropic upstream returned malformed tool input",
                        ));
                    }
                }
            };
            (
                id.clone(),
                call_id.clone(),
                name.clone(),
                *custom,
                state.output_index.expect("tool output index"),
                input,
            )
        };
        let arguments = tool_arguments(Some(&tool_input))?;
        let projected = if custom {
            custom_tool_input(&arguments)
        } else {
            arguments.clone()
        };
        let mut item = json!({
            "id": id, "type": if custom { "custom_tool_call" } else { "function_call" },
            "status": "completed", "call_id": call_id, "name": name
        });
        item[if custom { "input" } else { "arguments" }] = Value::String(projected.clone());
        {
            let state = self.blocks.get_mut(&raw_index).expect("tool block exists");
            state.item = Some(item.clone());
            state.closed = true;
        }
        let mut events = Vec::new();
        if custom {
            events.push(self.event(
                "response.custom_tool_call_input.delta",
                json!({
                    "type": "response.custom_tool_call_input.delta", "item_id": id,
                    "output_index": output_index, "delta": projected
                }),
            ));
        } else {
            events.push(self.event(
                "response.function_call_arguments.done",
                json!({
                    "type": "response.function_call_arguments.done", "item_id": id,
                    "output_index": output_index, "arguments": arguments
                }),
            ));
        }
        events.push(self.event(
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": output_index, "item": item}),
        ));
        Ok(events)
    }

    fn event(&mut self, event: &'static str, mut value: Value) -> crate::StreamEvent {
        self.sequence += 1;
        if let Some(value) = value.as_object_mut() {
            value.insert("sequence_number".to_owned(), Value::from(self.sequence));
        }
        crate::StreamEvent { event, value }
    }
}
