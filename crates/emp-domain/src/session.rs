//! Conversation ownership is metadata; original usage events and prices stay immutable.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct SessionScope(pub BTreeMap<String, SessionMetadata>);

#[derive(Default)]
pub struct SessionMetadata {
    pub name: String,
    pub kind: String,
    pub parents: BTreeSet<String>,
}

impl SessionScope {
    pub fn observe(&mut self, id: &str, parent: &str, kind: &str) {
        if !valid(id) {
            return;
        }
        if valid(parent) {
            self.0.entry(parent.into()).or_default();
        }
        let node = self.0.entry(id.into()).or_default();
        if valid(parent) {
            node.parents.insert(parent.into());
        }
        if matches!(
            kind,
            "conversation"
                | "subagent"
                | "review"
                | "sidechat"
                | "compaction"
                | "memory_extraction"
                | "memory_consolidation"
        ) {
            node.kind = kind.into();
        }
    }

    pub fn root(&self, id: &str) -> Option<&str> {
        let mut current = self.0.get_key_value(id)?;
        let mut visited = BTreeSet::new();
        for _ in 0..64 {
            if current.1.kind == "memory_consolidation" {
                return None;
            }
            if !visited.insert(current.0) || current.1.parents.len() > 1 {
                return None;
            }
            if let Some(parent) = current.1.parents.first() {
                current = self.0.get_key_value(parent)?;
            } else {
                return (current.1.kind == "conversation"
                    || (current.1.kind.is_empty() && !current.1.name.is_empty()))
                .then_some(current.0.as_str());
            }
        }
        None
    }
}

fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}
