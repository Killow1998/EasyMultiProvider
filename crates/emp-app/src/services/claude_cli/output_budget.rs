//! Recognize output exhaustion from protocol events, never from response text.
use serde_json::Value;

pub(super) const ERROR: &str = "claude_cli_output_budget_exhausted";

fn event_exhausted(event: &Value) -> bool {
    let reason = match event["type"].as_str() {
        Some("message") => &event["stop_reason"],
        Some("message_delta") => &event["delta"]["stop_reason"],
        Some("assistant") => return event_exhausted(&event["message"]),
        Some("stream_event") => return event_exhausted(&event["event"]),
        _ => return false,
    };
    matches!(reason.as_str(), Some("max_tokens" | "max_output_tokens"))
}

fn line_exhausted(line: &[u8]) -> bool {
    serde_json::from_slice(line.trim_ascii()).is_ok_and(|event| event_exhausted(&event))
}

pub(super) fn exhausted(body: &[u8]) -> bool {
    if let Ok(event) = serde_json::from_slice(body) {
        return event_exhausted(&event);
    }
    let mut parser = emp_transport::SseJsonParser::new();
    for chunk in body.chunks(8192) {
        let Ok(events) = parser.push(chunk) else {
            return false;
        };
        if events
            .into_iter()
            .any(|event| event_exhausted(&Value::Object(event)))
        {
            return true;
        }
    }
    parser.finish().is_ok_and(|events| {
        events
            .into_iter()
            .any(|event| event_exhausted(&Value::Object(event)))
    })
}

/// The process already retains bounded stdout; scan each complete line once.
#[derive(Default)]
pub(super) struct Events {
    consumed: usize,
    scanned: usize,
}

impl Events {
    pub(super) fn finish(&self, output: &[u8]) -> bool {
        line_exhausted(&output[self.consumed..])
    }

    pub(super) fn observe(&mut self, output: &[u8]) -> bool {
        while let Some(end) = output[self.scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = self.scanned + end;
            let exhausted = line_exhausted(&output[self.consumed..end]);
            self.consumed = end + 1;
            self.scanned = end + 1;
            if exhausted {
                return true;
            }
        }
        self.scanned = output.len();
        false
    }
}

#[cfg(test)]
#[path = "output_budget_tests.rs"]
mod tests;
