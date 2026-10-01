//! Close paired text/tool blocks and synthesize their final output events.
use super::super::response::{custom_tool_input, output_message, tool_arguments};
use super::super::{AnthropicError, upstream_error};
use super::{AnthropicBlockKind, AnthropicStream, tool_item};
use serde_json::{Map, Value, json};

impl AnthropicStream {
    pub(super) fn stop_block(
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

    pub(super) fn close_text(
        &mut self,
        raw_index: usize,
    ) -> Result<Vec<crate::StreamEvent>, AnthropicError> {
        let (id, output_index, text, item) = {
            let state = self.blocks.get_mut(&raw_index).expect("text block exists");
            let AnthropicBlockKind::Text { id, parts, .. } = &state.kind else {
                unreachable!()
            };
            let id = id.clone();
            let text = parts.concat();
            let output_index = state.output_index.expect("text output index");
            let item = output_message(&id, &text);
            state.item = Some(item.clone());
            state.closed = true;
            (id, output_index, text, item)
        };
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
        let item = tool_item(
            &id,
            custom,
            &call_id,
            &name,
            "completed",
            Value::String(projected.clone()),
        );
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
}
