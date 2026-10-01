//! Content block admission, delta parsing and bounded text/tool accumulation.
use super::super::response::custom_tool_id;
use super::super::{AnthropicError, required_upstream_string, upstream_error};
use super::{
    AnthropicBlockKind, AnthropicBlockState, AnthropicStream, MAX_ANTHROPIC_STREAM_TEXT_BYTES,
    MAX_ANTHROPIC_TOOL_INPUT_BYTES, tool_item,
};
use serde_json::{Map, Value, json};

impl AnthropicStream {
    /// Register a block. Only blocks with an output index participate in the
    /// ordered output stream; suppressed reasoning is stored but not ordered.
    fn insert_block_state(
        &mut self,
        raw_index: usize,
        output_index: Option<usize>,
        kind: AnthropicBlockKind,
    ) {
        self.blocks.insert(
            raw_index,
            AnthropicBlockState {
                output_index,
                closed: false,
                item: None,
                kind,
            },
        );
        if output_index.is_some() {
            self.ordered_blocks.push(raw_index);
        }
    }

    fn open_text_events(&mut self, id: &str, output_index: usize) -> Vec<crate::StreamEvent> {
        vec![
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
        ]
    }

    pub(super) fn start_block(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, self.blocks.len())?;
        if self.blocks.contains_key(&raw_index) {
            return Err(upstream_error(
                "Anthropic upstream repeated a content block",
            ));
        }
        let block = event
            .get("content_block")
            .and_then(Value::as_object)
            .ok_or_else(|| {
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
                self.insert_block_state(
                    raw_index,
                    Some(output_index),
                    AnthropicBlockKind::Text {
                        id: id.clone(),
                        parts: Vec::new(),
                        explicit: true,
                    },
                );
                let mut events = self.open_text_events(&id, output_index);
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
                self.insert_block_state(
                    raw_index,
                    Some(output_index),
                    AnthropicBlockKind::Tool {
                        id: id.clone(),
                        call_id: raw_id.to_owned(),
                        name: name.to_owned(),
                        custom,
                        initial_input,
                        json_parts: Vec::new(),
                        json_bytes: 0,
                    },
                );
                let item = tool_item(
                    &id,
                    custom,
                    raw_id,
                    name,
                    "in_progress",
                    Value::String(String::new()),
                );
                Ok(vec![self.event(
                    "response.output_item.added",
                    json!({"type": "response.output_item.added", "output_index": output_index, "item": item}),
                )])
            }
            "thinking" | "redacted_thinking" => {
                self.insert_block_state(raw_index, None, AnthropicBlockKind::SuppressedReasoning);
                Ok(Vec::new())
            }
            _ => Err(upstream_error(
                "Anthropic upstream returned an unsupported content block",
            )),
        }
    }

    pub(super) fn push_delta(
        &mut self,
        event: &Map<String, Value>,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let raw_index = Self::raw_index(event, 0)?;
        let delta = event
            .get("delta")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                upstream_error("Anthropic upstream returned an invalid content delta")
            })?;
        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
        if !self.blocks.contains_key(&raw_index) && delta_type == "text_delta" {
            let output_index = self.ordered_blocks.len();
            let id = self.ids.next_message();
            self.insert_block_state(
                raw_index,
                Some(output_index),
                AnthropicBlockKind::Text {
                    id: id.clone(),
                    parts: Vec::new(),
                    explicit: false,
                },
            );
            let mut events = self.open_text_events(&id, output_index);
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
}
