//! Bounded metadata-only lookup, including parents with no billed request of their own.
use super::{title_text, valid_id};
use emp_state::usage::ledger::SessionScope;
use rusqlite::{Connection, OpenFlags, params_from_iter};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

pub(super) fn read(home: &Path, scope: &mut SessionScope) {
    let mut databases = std::fs::read_dir(home)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let version = name
                .strip_prefix("state_")?
                .strip_suffix(".sqlite")?
                .parse::<u32>()
                .ok()?;
            let kind = entry.file_type().ok()?;
            (kind.is_file() && !kind.is_symlink()).then_some((version, entry.path()))
        })
        .collect::<Vec<_>>();
    databases.sort_by_key(|(version, _)| std::cmp::Reverse(*version));
    let mut described = BTreeSet::new();
    for (_, path) in databases.into_iter().take(4) {
        let Ok(connection) = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            continue;
        };
        let _ = connection.busy_timeout(std::time::Duration::from_millis(100));
        let columns = connection
            .prepare("PRAGMA table_info(threads)")
            .ok()
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(1))
                    .ok()
                    .map(|rows| rows.flatten().collect::<BTreeSet<_>>())
            })
            .unwrap_or_default();
        let column = |key: &str| {
            if columns.contains(key) {
                key.to_owned()
            } else {
                "NULL".into()
            }
        };
        let mut visited = BTreeSet::new();
        for _ in 0..64 {
            let pending = scope
                .0
                .keys()
                .filter(|id| valid_id(id) && !visited.contains(*id))
                .cloned()
                .collect::<Vec<_>>();
            if pending.is_empty() {
                break;
            }
            visited.extend(pending.iter().cloned());
            for ids in pending.chunks(100) {
                let slots = vec!["?"; ids.len()].join(",");
                let query = format!(
                    "SELECT id,{}, {},{} FROM threads WHERE id IN ({slots})",
                    column("title"),
                    column("source"),
                    column("thread_source")
                );
                if let Ok(mut statement) = connection.prepare(&query) {
                    let rows = statement.query_map(params_from_iter(ids), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    });
                    if let Ok(rows) = rows {
                        for (id, title, source, origin) in rows.flatten() {
                            let source: Value =
                                serde_json::from_str(source.as_deref().unwrap_or(""))
                                    .unwrap_or(Value::Null);
                            let parent = source
                                .pointer("/subagent/thread_spawn/parent_thread_id")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let mut kind = source_kind(origin.as_deref().unwrap_or(""), &source);
                            if kind == "conversation"
                                && scope.0.get(&id).is_some_and(|node| node.kind == "sidechat")
                            {
                                kind = "sidechat";
                            } else if kind == "conversation"
                                && source.is_null()
                                && origin.as_deref().is_none_or(str::is_empty)
                                && scope.0.get(&id).is_some_and(|node| !node.kind.is_empty())
                            {
                                // An older title-only database cannot contradict explicit request metadata.
                                kind = "";
                            }
                            if described.insert(id.clone()) {
                                scope.observe(&id, parent, kind);
                            }
                            let node = scope.0.entry(id).or_default();
                            if node.name.is_empty() {
                                node.name =
                                    title.as_deref().and_then(title_text).unwrap_or_default();
                            }
                        }
                    }
                }
                let query = format!(
                    "SELECT child_thread_id,parent_thread_id FROM thread_spawn_edges WHERE child_thread_id IN ({slots})"
                );
                if let Ok(mut statement) = connection.prepare(&query)
                    && let Ok(rows) = statement.query_map(params_from_iter(ids), |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                {
                    for (child, parent) in rows.flatten() {
                        let kind =
                            if scope.0.get(&child).is_none_or(|node| {
                                node.kind.is_empty() || node.kind == "conversation"
                            }) {
                                "subagent"
                            } else {
                                ""
                            };
                        scope.observe(&child, &parent, kind);
                    }
                }
            }
        }
    }
}

fn source_kind(origin: &str, source: &Value) -> &'static str {
    match origin {
        "user" => return "conversation",
        "guardian_review" | "review" => return "review",
        "sidechat" | "side_chat" => return "sidechat",
        "compact" | "compaction" => return "compaction",
        _ => {}
    }
    let Some(agent) = source.get("subagent") else {
        return "conversation";
    };
    let kind = agent
        .as_str()
        .or_else(|| agent["other"].as_str())
        .unwrap_or("");
    match kind {
        "guardian" | "review" | "auto_review" => "review",
        "compact" | "compaction" => "compaction",
        "sidechat" | "side_chat" => "sidechat",
        _ => "subagent",
    }
}
