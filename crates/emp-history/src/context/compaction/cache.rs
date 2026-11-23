use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type Fingerprint = [u8; 32];

struct Entry {
    prefix: Fingerprint,
    units: usize,
    summary: Arc<str>,
}

#[derive(Default)]
pub struct SummaryCache {
    entries: Mutex<VecDeque<Entry>>,
}

pub(super) fn prefixes(scope: &[&str], safe_budget: u64, units: &[Vec<Value>]) -> Vec<Fingerprint> {
    let mut hash = Sha256::new();
    hash.update(safe_budget.to_le_bytes());
    serde_json::to_writer(&mut hash, scope).expect("serialize history scope");
    units
        .iter()
        .map(|unit| {
            serde_json::to_writer(&mut hash, unit).expect("serialize visible history");
            hash.clone().finalize().into()
        })
        .collect()
}

impl SummaryCache {
    pub(super) fn longest_match(&self, prefixes: &[Fingerprint]) -> Option<(usize, Arc<str>)> {
        let entries = self.entries.lock().expect("history summaries");
        entries
            .iter()
            .filter(|entry| prefixes.get(entry.units - 1) == Some(&entry.prefix))
            .max_by_key(|entry| entry.units)
            .map(|entry| (entry.units, Arc::clone(&entry.summary)))
    }

    pub(super) fn insert(&self, prefix: Fingerprint, units: usize, summary: String) {
        const MAX_BYTES: usize = 8 * 1024 * 1024;
        if summary.len() > MAX_BYTES {
            return;
        }
        let mut entries = self.entries.lock().expect("history summaries");
        if entries.iter().any(|entry| entry.prefix == prefix) {
            return;
        }
        let mut bytes = summary.len()
            + entries
                .iter()
                .map(|entry| entry.summary.len())
                .sum::<usize>();
        while entries.len() >= 64 || bytes > MAX_BYTES {
            bytes -= entries.pop_front().expect("nonempty cache").summary.len();
        }
        entries.push_back(Entry {
            prefix,
            units,
            summary: summary.into(),
        });
    }
}
