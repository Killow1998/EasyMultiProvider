//! Conversation ownership is metadata; original usage events and prices stay immutable.
use super::*;
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
            // Global memory work may read many chats; its launcher is not its owner.
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

    pub(super) fn install(&self, connection: &Connection) -> Result<()> {
        connection.execute_batch(
            "CREATE TEMP TABLE conversation_scope (
             thread_id TEXT PRIMARY KEY,root_id TEXT NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL)",
        )?;
        let mut statement =
            connection.prepare("INSERT INTO conversation_scope VALUES (?,?,?,?)")?;
        for (id, node) in &self.0 {
            let root = self.root(id).unwrap_or("");
            let kind = if node.kind.is_empty() {
                if id == root {
                    "conversation"
                } else {
                    "related"
                }
            } else {
                &node.kind
            };
            statement.execute(params![id, root, node.name, kind])?;
        }
        Ok(())
    }
}

fn valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

impl UsageLedger {
    pub fn session_scope(&self, filter: &UsageSeriesFilter) -> Result<SessionScope> {
        let mut selected = filter.clone();
        selected.session = None;
        let (selection, args) = series::selection(&selected, false)?;
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let ids = rows(
            &connection,
            &format!(
                "SELECT DISTINCT e.session_id FROM {RESOLVED_USAGE} u
                JOIN usage_sessions e ON e.event_id=u.id WHERE {selection}"
            ),
            &args,
        )?;
        let mut scope = SessionScope::default();
        for id in &ids {
            scope.observe(id["session_id"].as_str().unwrap_or(""), "", "");
        }
        let mut visited = BTreeSet::new();
        for _ in 0..64 {
            let pending = scope
                .0
                .keys()
                .filter(|id| !visited.contains(*id))
                .cloned()
                .collect::<Vec<_>>();
            if pending.is_empty() {
                break;
            }
            visited.extend(pending.iter().cloned());
            let evidence = rows(
                &connection,
                "SELECT DISTINCT session_id,model_id,
                 json_extract(facts,'$.parent_thread_id') AS parent,
                 json_extract(facts,'$.session_kind') AS kind
                 FROM call_records WHERE session_id IN (SELECT value FROM json_each(?))",
                &[SqlValue::Text(json!(pending).to_string())],
            )?;
            for row in evidence {
                let model = row["model_id"].as_str().unwrap_or("");
                let kind = row["kind"].as_str().unwrap_or_else(|| {
                    if model == "codex-auto-review" || model.ends_with("/codex-auto-review") {
                        "review"
                    } else {
                        ""
                    }
                });
                scope.observe(
                    row["session_id"].as_str().unwrap_or(""),
                    row["parent"].as_str().unwrap_or(""),
                    kind,
                );
            }
        }
        Ok(scope)
    }
}
