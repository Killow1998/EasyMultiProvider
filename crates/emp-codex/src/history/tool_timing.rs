//! Cached, bounded projection of client-recorded tool intervals. Never reads outputs into reports.
use super::{CodexHomeHistoryReader, records::locate, visible::uuid_shape};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

const WINDOW: u64 = 8 * 1024 * 1024;
type Turns = BTreeMap<String, u64>;
type Signature = (u64, Option<SystemTime>);

#[derive(Default)]
pub struct ToolTimings {
    cache: Mutex<BTreeMap<(PathBuf, String), (Signature, Turns)>>,
}

impl ToolTimings {
    pub fn resolve(
        &self,
        home: &Path,
        queries: &[(String, String)],
    ) -> BTreeMap<(String, String), u64> {
        let reader = CodexHomeHistoryReader::new(home);
        let Ok(database) = reader.latest_state_database() else {
            return BTreeMap::new();
        };
        let threads = queries
            .iter()
            .map(|(thread, _)| thread)
            .collect::<BTreeSet<_>>();
        let mut result = BTreeMap::new();
        let Ok(mut cache) = self.cache.lock() else {
            return result;
        };
        for thread in threads.into_iter().filter(|id| uuid_shape(id)).take(16) {
            let Ok(location) = locate(&database, thread) else {
                continue;
            };
            let Ok(path) = reader.contained_rollout_path(&location.path) else {
                continue;
            };
            // Compressed/paginated fragments without explicit timing stay unknown.
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Ok(meta) = path.metadata() else { continue };
            let signature = (meta.len(), meta.modified().ok());
            let key = (path.clone(), thread.to_string());
            if cache.get(&key).is_none_or(|(old, _)| *old != signature) {
                if cache.len() >= 16 {
                    cache.clear();
                }
                let Some(turns) = read(&path, thread, meta.len()) else {
                    continue;
                };
                cache.insert(key.clone(), (signature, turns));
            }
            if let Some((_, turns)) = cache.get(&key) {
                for (session, turn) in queries.iter().filter(|(session, _)| session == thread) {
                    if let Some(duration) = turns.get(turn) {
                        result.insert((session.clone(), turn.clone()), *duration);
                    }
                }
            }
        }
        result
    }
}

#[derive(Default)]
struct Turn {
    started: bool,
    incomplete: bool,
    intervals: BTreeMap<String, (u64, u64)>,
}

fn read(path: &Path, thread: &str, size: u64) -> Option<Turns> {
    let mut file = File::open(path).ok()?;
    let start = size.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(WINDOW).read_to_end(&mut bytes).ok()?;
    let mut turns = BTreeMap::<String, Turn>::new();
    for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if (start > 0 && index == 0) || !line.ends_with(b"\n") {
            continue;
        }
        if ![b"\"item_completed\"".as_slice(), b"\"task_started\""]
            .iter()
            .any(|marker| line.windows(marker.len()).any(|part| part == *marker))
        {
            continue;
        }
        if line.len() > 1024 * 1024 {
            return None;
        }
        let record = serde_json::from_slice::<Value>(line).ok()?;
        if record["type"] != "event_msg" {
            continue;
        }
        let payload = &record["payload"];
        let Some(id) = payload["turn_id"].as_str().filter(|id| uuid_shape(id)) else {
            continue;
        };
        if !turns.contains_key(id) && turns.len() >= 128 {
            continue;
        }
        let turn = turns.entry(id.into()).or_default();
        if payload["type"] == "task_started" {
            turn.started = true;
            continue;
        }
        if payload["thread_id"] != thread {
            turn.incomplete = true;
            continue;
        }
        let item = &payload["item"];
        if !matches!(
            item["type"].as_str(),
            Some(
                "CommandExecution"
                    | "FileChange"
                    | "McpToolCall"
                    | "DynamicToolCall"
                    | "CollabAgentToolCall"
                    | "WebSearch"
                    | "ImageView"
                    | "ImageGeneration"
                    | "Extension"
            )
        ) {
            continue;
        }
        let interval = payload["started_at_ms"]
            .as_u64()
            .zip(payload["completed_at_ms"].as_u64())
            .filter(|(from, to)| {
                *from > 0 && to.checked_sub(*from).is_some_and(|ms| ms <= 86_400_000)
            });
        let key = item["id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256);
        if let Some((key, interval)) = key.zip(interval).filter(|_| turn.intervals.len() < 4096) {
            turn.intervals.insert(key.into(), interval);
        } else {
            turn.incomplete = true;
        }
    }
    Some(
        turns
            .into_iter()
            .filter_map(|(id, turn)| {
                if !turn.started || turn.incomplete {
                    return None;
                }
                let mut intervals = turn.intervals.into_values().collect::<Vec<_>>();
                intervals.sort_unstable();
                let (mut total, mut until) = (0, 0);
                for (from, to) in intervals {
                    total += to.saturating_sub(from.max(until));
                    until = until.max(to);
                }
                Some((id, total))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
