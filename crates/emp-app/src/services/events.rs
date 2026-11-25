use emp_transport::SseJsonParser;
use serde_json::{Map, Value};

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn sse_frame(event: &str, body: &Value) -> Result<Vec<u8>, serde_json::Error> {
    let mut frame = Vec::with_capacity(event.len() + 128);
    frame.extend_from_slice(b"event: ");
    frame.extend_from_slice(event.as_bytes());
    frame.extend_from_slice(b"\ndata: ");
    crate::util::spaced_json::write(&mut frame, body)?;
    frame.extend_from_slice(b"\n\n");
    Ok(frame)
}

pub(crate) fn stream_event_activity(event: &Value) -> (bool, bool) {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let item = event.get("item").and_then(Value::as_object);
    let item_type = item
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_activity = matches!(
        item_type,
        "function_call" | "custom_tool_call" | "tool_call" | "tool_search_call"
    ) || event_type.contains("function_call")
        || event_type.contains("tool_call");
    let mut output_emitted = tool_activity;
    if event_type.ends_with(".delta") || event_type.ends_with(".done") {
        output_emitted |= [
            "output_text",
            "output_image",
            "image_generation",
            "reasoning",
        ]
        .iter()
        .any(|marker| event_type.contains(marker));
    } else {
        output_emitted |=
            event_type.contains("output_image") || event_type.contains("image_generation");
    }
    output_emitted |= event
        .get("part")
        .and_then(Value::as_object)
        .and_then(|part| part.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|part_type| {
            matches!(
                part_type,
                "output_text" | "output_image" | "reasoning_text" | "summary_text"
            )
        });
    if let Some(content) = item
        .and_then(|item| item.get("content"))
        .and_then(Value::as_array)
    {
        output_emitted |= content.iter().any(|part| {
            part.get("type")
                .and_then(Value::as_str)
                .is_some_and(|part_type| {
                    matches!(
                        part_type,
                        "output_text" | "output_image" | "image" | "image_url" | "reasoning_text"
                    )
                })
        });
    }
    (output_emitted, tool_activity)
}

#[derive(Clone, Copy)]
pub(crate) enum TerminalEvent {
    Completed,
    Incomplete,
    Failed,
    Error,
    ReportResult,
}

impl TerminalEvent {
    pub(crate) fn response(name: &[u8]) -> Option<Self> {
        match name {
            b"response.completed" => Some(Self::Completed),
            b"response.incomplete" => Some(Self::Incomplete),
            b"response.failed" => Some(Self::Failed),
            b"error" => Some(Self::Error),
            _ => None,
        }
    }

    pub(crate) fn response_outcome(self) -> Option<&'static str> {
        match self {
            Self::Completed => Some("completed"),
            Self::Incomplete => Some("incomplete"),
            Self::Failed => Some("failed"),
            Self::Error => Some("error"),
            Self::ReportResult => None,
        }
    }
}

pub(crate) fn terminal_stream_event(event: &Value) -> bool {
    event["type"]
        .as_str()
        .and_then(|name| TerminalEvent::response(name.as_bytes()))
        .is_some()
}

fn terminal_sse_event(frame: &SseFrame) -> Option<TerminalEvent> {
    if frame.has_name {
        if frame.name == b"result" {
            return Some(TerminalEvent::ReportResult);
        }
        return TerminalEvent::response(&frame.name);
    }
    let name = frame.data.as_ref()?.get("type")?.as_str()?;
    TerminalEvent::response(name.as_bytes())
}

pub(crate) struct SseFrame {
    observe_json: bool,
    line: Vec<u8>,
    name: Vec<u8>,
    has_name: bool,
    json: Option<SseJsonParser>,
    data: Option<Map<String, Value>>,
}

impl SseFrame {
    pub(crate) fn new(observe_json: bool) -> Self {
        Self {
            observe_json,
            line: Vec::new(),
            name: Vec::new(),
            has_name: false,
            json: Some(SseJsonParser::new()),
            data: None,
        }
    }

    pub(crate) fn push(
        &mut self,
        chunk: &[u8],
        mut completed: impl FnMut(Option<TerminalEvent>, Option<Map<String, Value>>),
    ) {
        const LINE_PREFIX_LIMIT: usize = b"event: response.incomplete\r".len() + 1;
        let mut start = 0;
        let mut end = 0;
        for segment in chunk.split_inclusive(|&byte| byte == b'\n') {
            end += segment.len();
            let line = segment.strip_suffix(b"\n").unwrap_or(segment);
            let available = LINE_PREFIX_LIMIT - self.line.len();
            self.line
                .extend_from_slice(&line[..line.len().min(available)]);
            if line.len() == segment.len() {
                continue;
            }
            let line = self.line.strip_suffix(b"\r").unwrap_or(&self.line);
            if line.is_empty() {
                if self.observe_json || !self.has_name {
                    self.data = self.push_json(&chunk[start..end]).pop();
                }
                completed(terminal_sse_event(self), self.data.take());
                self.has_name = false;
                self.json.get_or_insert_with(SseJsonParser::new);
                start = end;
            } else if let Some(name) = line.strip_prefix(b"event:") {
                self.has_name = true;
                self.name.clear();
                self.name
                    .extend_from_slice(name.strip_prefix(b" ").unwrap_or(name));
            }
            self.line.clear();
        }
        if start < end && (self.observe_json || !self.has_name) {
            self.push_json(&chunk[start..]);
        }
    }

    fn push_json(&mut self, chunk: &[u8]) -> Vec<Map<String, Value>> {
        if let Some(parser) = self.json.as_mut() {
            match parser.push(chunk) {
                Ok(events) => return events,
                Err(_) => self.json = None,
            }
        }
        Vec::new()
    }
}

/// Codex extensions are not part of the public Responses event protocol.
pub(crate) fn codex_client(headers: &std::collections::BTreeMap<String, String>) -> bool {
    headers.iter().any(|(key, value)| {
        matches!(
            key.to_ascii_lowercase().as_str(),
            "originator" | "user-agent"
        ) && value.to_ascii_lowercase().contains("codex")
    })
}

pub(crate) fn codex_event(kind: &str) -> bool {
    kind.starts_with("codex.") || matches!(kind, "response.metadata" | "keepalive")
}
