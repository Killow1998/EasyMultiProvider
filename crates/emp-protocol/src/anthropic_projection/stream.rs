//! Anthropic SSE stream state and event synthesis.

use super::response::{anthropic_usage, incomplete_reason};
use super::{AnthropicError, python_truthy, stream_error, upstream_error};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

mod blocks;
mod completion;

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
        let event = event
            .as_object()
            .ok_or_else(|| upstream_error("Anthropic upstream returned an invalid stream event"))?;
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "error" {
            return Err(upstream_error("Anthropic upstream returned an error event"));
        }
        match event_type {
            "message_start" => {
                if let Some(value) = event
                    .get("message")
                    .and_then(Value::as_object)
                    .and_then(|message| message.get("usage"))
                    .and_then(Value::as_object)
                {
                    self.usage.extend(value.clone());
                }
                Ok(Vec::new())
            }
            "message_delta" => {
                if let Some(value) = event.get("usage").and_then(Value::as_object) {
                    self.usage.extend(value.clone());
                }
                if let Some(reason) = event
                    .get("delta")
                    .and_then(Value::as_object)
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

    fn event(&mut self, event: &'static str, mut value: Value) -> crate::StreamEvent {
        self.sequence += 1;
        if let Some(value) = value.as_object_mut() {
            value.insert("sequence_number".to_owned(), Value::from(self.sequence));
        }
        crate::StreamEvent { event, value }
    }
}
fn tool_item(
    id: &str,
    custom: bool,
    call_id: &str,
    name: &str,
    status: &'static str,
    payload: Value,
) -> Value {
    let mut item = json!({
        "id": id, "type": if custom { "custom_tool_call" } else { "function_call" },
        "status": status, "call_id": call_id, "name": name
    });
    item[if custom { "input" } else { "arguments" }] = payload;
    item
}
