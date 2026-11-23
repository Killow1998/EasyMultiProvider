//! Materialize only the visible prefix hidden by one native compaction item;
//! never write a parallel transcript or copy hidden reasoning.

use serde_json::{Map, Value};
use std::fmt;

pub mod context;
mod diagnostic;
pub use diagnostic::HistoryDiagnostic;

pub const ACTIVE_INPUT_START: &str = "_emp_active_input_start";
const COMPACTION_PREFIX: &str = "emp1:";
pub(crate) const CHECKPOINT_PREFIX: &str = "Portable checkpoint from Codex-visible local history. Continue from this state without repeating completed work.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryError {
    reason: String,
}

impl HistoryError {
    pub fn new(reason: impl AsRef<str>) -> Self {
        let reason = reason
            .as_ref()
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '_' {
                    character
                } else {
                    '_'
                }
            })
            .take(64)
            .collect::<String>();
        Self {
            reason: if reason.is_empty() {
                "history_unavailable".to_owned()
            } else {
                reason
            },
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "history_reconstruction_failed: reason={}; Codex history was not modified",
            self.reason
        )
    }
}

impl std::error::Error for HistoryError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryAnchor {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub window_id: Option<String>,
    pub forked_from_thread_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleItem {
    pub kind: String,
    pub content: Value,
    pub item_id: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
    pub raw_type: Option<String>,
}

impl VisibleItem {
    pub fn replay_key(&self) -> Option<(&str, &str)> {
        Some((
            self.raw_type.as_deref()?,
            self.call_id.as_deref().or(self.item_id.as_deref())?,
        ))
    }

    pub fn new(kind: impl Into<String>, content: Value) -> Self {
        Self {
            kind: kind.into(),
            content,
            item_id: None,
            turn_id: None,
            call_id: None,
            raw_type: None,
        }
    }
}

pub fn wire_replay_key(item: &Map<String, Value>) -> Option<(&str, &str)> {
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    let id = ["call_id", "callId", "id", "item_id", "itemId"]
        .iter()
        .find_map(|key| {
            item.get(*key)?
                .as_str()
                .map(str::trim)
                .filter(|id| !id.is_empty())
        })?;
    Some((kind, id))
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistorySnapshot {
    pub thread_id: String,
    pub items: Vec<VisibleItem>,
    pub source_model: Option<String>,
}

pub trait HistoryReader {
    fn read_compaction_history(
        &self,
        anchor: &HistoryAnchor,
        compaction: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError>;
}

mod anchor;
mod preparation;
#[cfg(test)]
mod tests;
mod tool_pairs;
mod wire;

pub use anchor::request_history_anchor;
pub use preparation::{build_compaction_replacement, prepare, prepare_owned};
