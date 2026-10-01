//! Python-compatible projection for external Responses providers.
//!
//! External gateways receive a complete, stateless history. Plaintext
//! reasoning is never returned to Codex, while explicitly enabled opaque state
//! and bounded summaries can cross the protocol boundary.

use serde_json::Value;
use std::fmt;

pub(crate) const COMPACTION_PREFIX: &str = "emp1:";
pub(crate) const COMPACTION_SUMMARY_PREFIX: &str =
    "Another model produced a continuation summary. Continue from this summary:";
const PLAINTEXT_REASONING_FIELDS: &[&str] = &[
    "reasoning",
    "reasoning_text",
    "reasoning_content",
    "thinking",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortableProjectionError {
    index: usize,
    item_type: String,
    part_types: Vec<String>,
    failure_class: String,
}

impl PortableProjectionError {
    pub(crate) fn new(
        index: usize,
        item_type: &str,
        part_types: Vec<String>,
        failure_class: &str,
    ) -> Self {
        Self {
            index,
            item_type: item_type.chars().take(64).collect(),
            part_types: part_types
                .into_iter()
                .take(16)
                .map(|part| part.chars().take(64).collect())
                .collect(),
            failure_class: failure_class.chars().take(64).collect(),
        }
    }

    pub const fn index(&self) -> usize {
        self.index
    }

    pub fn item_type(&self) -> &str {
        &self.item_type
    }

    pub fn part_types(&self) -> &[String] {
        &self.part_types
    }

    pub fn failure_class(&self) -> &str {
        &self.failure_class
    }
}

impl fmt::Display for PortableProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts = if self.part_types.is_empty() {
            "none".to_owned()
        } else {
            self.part_types.join(",")
        };
        write!(
            formatter,
            "request projection failed: index={} type={} parts={} class={}",
            self.index, self.item_type, parts, self.failure_class
        )
    }
}

impl std::error::Error for PortableProjectionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponsesValidationError {
    message: &'static str,
}

impl ResponsesValidationError {
    pub(crate) const fn new(message: &'static str) -> Self {
        Self { message }
    }

    pub const fn message(self) -> &'static str {
        self.message
    }
}

impl fmt::Display for ResponsesValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ResponsesValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalObservation {
    pub status: u16,
    pub success: bool,
    pub error_class: &'static str,
}

fn error(index: usize, item_type: &str, failure_class: &str) -> PortableProjectionError {
    PortableProjectionError::new(index, item_type, Vec::new(), failure_class)
}

fn python_string(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None | Some(Value::Null) => fallback.to_owned(),
        Some(Value::String(value)) => value.clone(),
        Some(Value::Bool(value)) => if *value { "True" } else { "False" }.to_owned(),
        Some(Value::Number(value)) => value.to_string(),
        Some(v @ (Value::Array(_) | Value::Object(_))) => {
            serde_json::to_string(v).unwrap_or_else(|_| fallback.into())
        }
    }
}

mod input;
mod request;
mod response;
mod stream;
mod tools;
mod validation;
pub(crate) use input::decode_compaction;
pub use request::project_request;
pub use response::{custom_tool_names, project_response};
pub use stream::PortableStreamProjector;
pub use validation::{terminal_observation, validate_responses_body};
