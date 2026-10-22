//! Read-only names from Codex's own index. No rollout/body scans or child processes.
use emp_state::usage::ledger::SessionScope;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;
mod database;
#[cfg(test)]
#[path = "session_directory/tests.rs"]
mod lineage_tests;

type Signature = (PathBuf, Option<SystemTime>, u64);
#[derive(Default)]
pub struct SessionDirectory {
    index: Mutex<Option<(Signature, BTreeMap<String, String>)>>,
}
impl SessionDirectory {
    pub fn resolve(&self, home: &Path, ids: &[String]) -> BTreeMap<String, String> {
        let mut scope = SessionScope::default();
        for id in ids {
            scope.observe(id, "", "");
        }
        self.enrich(home, &mut scope);
        ids.iter()
            .filter_map(|id| {
                scope
                    .0
                    .get(id)
                    .filter(|node| !node.name.is_empty())
                    .map(|node| (id.clone(), node.name.clone()))
            })
            .collect()
    }

    pub fn enrich(&self, home: &Path, scope: &mut SessionScope) {
        let Ok(home) = home.canonicalize() else {
            return;
        };
        database::read(&home, scope);
        let path = home.join("session_index.jsonl");
        if let Ok(meta) = std::fs::symlink_metadata(&path)
            && meta.is_file()
            && !meta.is_symlink()
            && let Ok(mut cache) = self.index.lock()
        {
            let signature = (path.clone(), meta.modified().ok(), meta.len());
            if cache.as_ref().is_none_or(|(old, _)| *old != signature) {
                *cache = Some((signature, read_index(&path, &meta)));
            }
            if let Some((_, index)) = &*cache {
                for (id, node) in &mut scope.0 {
                    if let Some(name) = index.get(id) {
                        node.name.clone_from(name);
                    }
                }
            }
        }
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}
fn title_text(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && value.len() <= 4096).then(|| value.to_owned())
}
fn read_index(path: &Path, metadata: &Metadata) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    let Ok(mut file) = File::open(path) else {
        return names;
    };
    let start = metadata.len().saturating_sub(8 * 1024 * 1024);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return names;
    }
    let mut bytes = Vec::new();
    if file.take(8 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return names;
    }
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if (start > 0 && index == 0) || line.len() > 16 * 1024 {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(id) = value["id"].as_str().filter(|id| valid_id(id)) else {
            continue;
        };
        let Some(name) = value["thread_name"]
            .as_str()
            .or_else(|| value["title"].as_str())
            .and_then(title_text)
        else {
            continue;
        };
        names.insert(id.into(), name);
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_read_only_cached_and_prefer_the_latest_explicit_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session_index.jsonl");
        std::fs::write(
            &path,
            b"{\"id\":\"thread-one\",\"thread_name\":\"Old name\"}\n",
        )
        .unwrap();
        let database = root.path().join("state_5.sqlite");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,title TEXT); INSERT INTO threads VALUES('thread-two','Database title')").unwrap();
        drop(connection);
        let directory = SessionDirectory::default();
        let ids = vec![
            "thread-one".into(),
            "thread-two".into(),
            "../auth.json".into(),
        ];
        assert_eq!(directory.resolve(root.path(), &ids).len(), 2);
        std::fs::write(
            &path,
            b"{\"id\":\"thread-one\",\"thread_name\":\"New longer name\"}\n",
        )
        .unwrap();
        let names = directory.resolve(root.path(), &ids);
        assert_eq!(names["thread-one"], "New longer name");
        assert_eq!(names["thread-two"], "Database title");
        assert!(!names.contains_key("../auth.json"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"id\":\"thread-one\",\"thread_name\":\"New longer name\"}\n"
        );
    }
}
