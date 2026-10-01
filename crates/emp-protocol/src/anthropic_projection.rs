//! Bounded Responses <-> Anthropic Messages projections.

use serde_json::Value;

const COMPACTION_PREFIX: &str = "emp1:";
const COMPACTION_SUMMARY_PREFIX: &str = "Another language model started this task and produced a continuation summary. Use it to continue without repeating completed work:";
const ANTHROPIC_IMAGE_MEDIA_TYPES: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];
const TEXT_PART_TYPES: [&str; 3] = ["input_text", "output_text", "text"];
const OMITTED_INPUT_TYPES: [&str; 2] = ["reasoning", "additional_tools"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnthropicErrorKind {
    Request,
    HistoryReconstruction,
    Upstream,
    StreamIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthropicError {
    pub kind: AnthropicErrorKind,
    pub message: &'static str,
}

impl std::fmt::Display for AnthropicError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for AnthropicError {}

impl AnthropicError {
    pub const fn status(&self) -> u16 {
        match self.kind {
            AnthropicErrorKind::Request => 422,
            AnthropicErrorKind::HistoryReconstruction => 409,
            AnthropicErrorKind::Upstream | AnthropicErrorKind::StreamIncomplete => 502,
        }
    }

    pub const fn error_class(&self) -> &'static str {
        match self.kind {
            AnthropicErrorKind::Request => "invalid_request",
            AnthropicErrorKind::HistoryReconstruction => "history_reconstruction_failed",
            AnthropicErrorKind::Upstream => "protocol_error",
            AnthropicErrorKind::StreamIncomplete => "stream_incomplete",
        }
    }
}

fn request_error(message: &'static str) -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::Request,
        message,
    }
}

fn upstream_error(message: &'static str) -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::Upstream,
        message,
    }
}

fn history_error() -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::HistoryReconstruction,
        message: "history_reconstruction_failed: reason=history_projection_incomplete; Codex history was not modified",
    }
}

fn stream_error(message: &'static str) -> AnthropicError {
    AnthropicError {
        kind: AnthropicErrorKind::StreamIncomplete,
        message,
    }
}

fn required_string<'a>(
    value: Option<&'a Value>,
    message: &'static str,
) -> Result<&'a str, AnthropicError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(request_error(message))
}

fn required_upstream_string<'a>(
    value: Option<&'a Value>,
    message: &'static str,
) -> Result<&'a str, AnthropicError> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(upstream_error(message))
}

fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null | Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

mod input;
mod request;
mod response;
mod stream;
mod tools;
pub use request::responses_to_anthropic;
pub use response::{anthropic_usage, response_from_anthropic};
pub use stream::{AnthropicIds, AnthropicStream};
