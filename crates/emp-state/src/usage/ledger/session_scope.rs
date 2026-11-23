use super::*;
use std::collections::BTreeSet;

pub(super) fn install(scope: &SessionScope, connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE conversation_scope (
             thread_id TEXT PRIMARY KEY,root_id TEXT NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL)",
    )?;
    let mut statement = connection.prepare("INSERT INTO conversation_scope VALUES (?,?,?,?)")?;
    for (id, node) in &scope.0 {
        let root = scope.root(id).unwrap_or("");
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

impl UsageLedger {
    pub fn session_scope(
        &self,
        filter: &UsageSeriesFilter,
        cancelled: Option<Arc<AtomicBool>>,
    ) -> Result<SessionScope> {
        let mut selected = filter.clone();
        selected.session = None;
        let (selection, args) = series::selection(&selected, false)?;
        let _guard = self.lock.lock().map_err(|_| UsageError)?;
        let connection = self.connect()?;
        let connection = read_cache::QueryConnection::new(&connection, cancelled)?;
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
