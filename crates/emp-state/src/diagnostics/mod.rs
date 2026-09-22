//! Bounded, content-free route observations shared by diagnostics and the UI.
pub mod analytics;
mod journal;
pub mod schema;
pub use journal::Journal;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Mutex;
pub const CAPACITY: usize = 512;

pub struct Diagnostics {
    pub journal: Journal,
    recent: Mutex<VecDeque<Value>>,
}
impl Diagnostics {
    pub fn new(state_root: &Path) -> Self {
        Self {
            journal: Journal::new(state_root.join("logs")),
            recent: Mutex::new(VecDeque::new()),
        }
    }
    pub fn record(&self, event: &Value) {
        let record = schema::route_record(event, &self.journal);
        self.journal.event("info", "route_observation", &record);
        let mut recent = self.recent.lock().expect("diagnostic observations");
        if recent.len() == CAPACITY {
            recent.pop_front();
        }
        recent.push_back(record);
    }
    pub fn snapshot(&self) -> Value {
        let persisted = self.journal.read_routes(CAPACITY);
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
        result["records"] = json!(&records[records.len().saturating_sub(64)..]);
        result
    }
}

pub fn timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.microsecond()
    )
}
pub(crate) fn random_id(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    if getrandom::getrandom(&mut value).is_err() {
        return String::new();
    }
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
