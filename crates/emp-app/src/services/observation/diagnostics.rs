//! Bounded, content-free route observations shared by diagnostics and the UI.
mod analytics;
mod recovery;
pub(crate) mod schema;
use emp_storage::diagnostics::{Journal, is_probe, random_id, timestamp};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;
const CAPACITY: usize = 512;

pub(crate) struct Diagnostics {
    pub(crate) journal: Journal,
    recent: Mutex<VecDeque<Value>>,
}
impl Diagnostics {
    pub(crate) fn new(state_root: &Path) -> Self {
        Self {
            journal: Journal::new(state_root.join("logs")),
            recent: Mutex::new(VecDeque::new()),
        }
    }
    pub(crate) fn record(&self, event: &Value) {
        let record = schema::route_record(event, &self.journal);
        self.journal.event("info", "route_observation", &record);
        if is_probe(record["route"].as_str().unwrap_or("")) {
            return;
        }
        let mut recent = self.recent.lock().expect("diagnostic observations");
        if recent.len() == CAPACITY {
            recent.pop_front();
        }
        recent.push_back(record);
    }
    /// Called at request start; a slow generation must not age out a prompt recovery.
    /// Accepts a sanitized route record and consults only the bounded in-memory ring.
    pub(crate) fn continuation_recovery_from(&self, record: &Value) -> Option<String> {
        let recent = self.recent.lock().expect("diagnostic observations");
        recovery::previous_rejection(&recent, record).map(str::to_owned)
    }
    pub(crate) fn snapshot(&self) -> Value {
        let (persisted, probes) = self.journal.read_observations(CAPACITY);
        let recent = self.recent.lock().expect("diagnostic observations");
        let mut records = VecDeque::new();
        let mut seen = HashSet::new();
        for record in persisted
            .iter()
            .map(|event| schema::route_record(event, &self.journal))
            .chain(recent.iter().cloned())
        {
            let identity = record["observation_id"].as_str().unwrap_or("").to_owned();
            if seen.insert(identity) {
                if records.len() == CAPACITY {
                    records.pop_front();
                }
                records.push_back(record);
            }
        }
        let records: Vec<_> = records.into_iter().collect();
        let mut result = analytics::summarize(&records, time::OffsetDateTime::now_utc());
        result["capacity"] = json!(CAPACITY);
        result["sample_count"] = json!(records.len());
        result["continuation_recovery"] = recovery::summary(&records);
        result["records"] = json!(&records[records.len().saturating_sub(64)..]);
        result["probes"] = json!(probes);
        result
    }
}
