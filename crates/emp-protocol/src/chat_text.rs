//! Keep native DeepSeek tool envelopes out of the Chat text channel.

use super::{ProtocolError, protocol_error};

const TOOL_MARKERS: [&str; 2] = ["<｜DSML｜", "</｜DSML｜"];

#[derive(Clone, Default)]
pub(super) struct ChatText {
    pending: String,
    code_ticks: usize,
    ticks: usize,
}

impl ChatText {
    pub(super) fn push(&mut self, text: &str) -> Result<String, ProtocolError> {
        let mut visible = String::with_capacity(text.len());
        for character in text.chars() {
            if character == '`' {
                self.ticks += 1;
            } else if self.ticks > 0 {
                if self.code_ticks == 0 {
                    self.code_ticks = self.ticks;
                } else if self.ticks == self.code_ticks
                    || (self.code_ticks >= 3 && self.ticks >= self.code_ticks)
                {
                    self.code_ticks = 0;
                }
                self.ticks = 0;
            }
            if character == '`' || self.code_ticks > 0 {
                visible.push_str(&self.finish());
                visible.push(character);
                continue;
            }
            if self.pending.is_empty() && character != '<' {
                visible.push(character);
                continue;
            }
            self.pending.push(character);
            while !self.pending.is_empty()
                && !TOOL_MARKERS
                    .iter()
                    .any(|marker| marker.starts_with(&self.pending))
            {
                visible.push(self.pending.remove(0));
            }
            if TOOL_MARKERS.contains(&self.pending.as_str()) {
                return Err(protocol_error(
                    "Chat Completions upstream returned DSML tool markup in content instead of structured tool_calls",
                ));
            }
        }
        Ok(visible)
    }

    pub(super) fn finish(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}
