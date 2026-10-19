//! Owns bounded, memory-only provenance and coalesced recovery results.
use super::{DestinationPrepareError, message};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const RETENTION: Duration = Duration::from_secs(3600);
// Keep bounded source fingerprints for long chats, separately from plaintext.
const ORIGIN_RETENTION: Duration = Duration::from_secs(24 * 3600);
type Key = [u8; 32];

#[derive(Clone)]
pub(super) struct Origin {
    pub(super) model: String,
    pub(super) owner: String,
    at: Instant,
}

struct Recovered {
    key: Key,
    text: String,
    at: Instant,
}

#[derive(Default)]
pub(crate) struct CollaborationState {
    origins: Mutex<BTreeMap<Key, Origin>>,
    // Serializes only recovery, including duplicate concurrent requests. Ordinary
    // Native and external turns never take this lock; no plaintext is persisted.
    recovered: Mutex<VecDeque<Recovered>>,
}

fn key(text: &[u8]) -> Key {
    Sha256::digest(text).into()
}

impl CollaborationState {
    pub(crate) fn observe(&self, model: &str, owner: &str, event: &Value) {
        if owner.is_empty() || owner.starts_with("unconfirmed:") {
            return;
        }
        let response = event.get("response").unwrap_or(event);
        let items: Vec<&Value> = if event["type"] == "response.output_item.done" {
            vec![&event["item"]]
        } else {
            response["output"]
                .as_array()
                .into_iter()
                .flatten()
                .collect()
        };
        for item in items {
            if item["type"] != "function_call"
                || item["namespace"] != "collaboration"
                || !matches!(
                    item["name"].as_str(),
                    Some("spawn_agent" | "send_message" | "followup_task")
                )
            {
                continue;
            }
            let Some(arguments) = item["arguments"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            else {
                continue;
            };
            let Some(ciphertext) = arguments["message"]
                .as_str()
                .filter(|s| message::ciphertext(s))
            else {
                continue;
            };
            let mut origins = self.origins.lock().unwrap();
            origins.retain(|_, origin| origin.at.elapsed() < ORIGIN_RETENTION);
            let id = key(ciphertext.as_bytes());
            if origins.contains_key(&id) {
                continue;
            }
            if origins.len() >= 4096
                && let Some(oldest) = origins
                    .iter()
                    .min_by_key(|(_, origin)| origin.at)
                    .map(|(key, _)| *key)
            {
                origins.remove(&oldest);
            }
            origins.insert(
                id,
                Origin {
                    model: model.into(),
                    owner: owner.into(),
                    at: Instant::now(),
                },
            );
        }
    }

    pub(super) fn origin(&self, ciphertext: &str) -> Option<Origin> {
        self.origins
            .lock()
            .unwrap()
            .get(&key(ciphertext.as_bytes()))
            .filter(|origin| origin.at.elapsed() < ORIGIN_RETENTION)
            .cloned()
    }

    pub(super) fn recover(
        &self,
        owner: &str,
        endpoint: &str,
        item: &Value,
        execute: impl FnOnce() -> Result<String, DestinationPrepareError>,
    ) -> Result<String, DestinationPrepareError> {
        let id = key(&serde_json::to_vec(&json!([owner, endpoint, item])).expect("task JSON"));
        let mut cache = self.recovered.lock().unwrap();
        cache.retain(|entry| entry.at.elapsed() < RETENTION);
        if let Some(entry) = cache.iter().find(|entry| entry.key == id) {
            return Ok(entry.text.clone());
        }
        let text = execute()?;
        if cache.len() == 64 {
            cache.pop_front();
        }
        cache.push_back(Recovered {
            key: id,
            text: text.clone(),
            at: Instant::now(),
        });
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_fingerprints_survive_two_hour_chats_but_are_bounded_and_expire() {
        let state = CollaborationState::default();
        let cipher = format!("gAAAA{}", "A".repeat(100));
        let event = |text: &str| {
            json!({"type":"response.output_item.done","item":{
                "type":"function_call","namespace":"collaboration","name":"send_message",
                "arguments":json!({"target":"/root","message":text}).to_string()
            }})
        };
        state.observe("original/gpt", "original-owner", &event(&cipher));
        let id = key(cipher.as_bytes());
        state.origins.lock().unwrap().get_mut(&id).unwrap().at =
            Instant::now() - Duration::from_secs(2 * 3600);
        state.observe(
            "original/gpt",
            "original-owner",
            &event(&format!("gAAAA{}", "B".repeat(100))),
        );
        let origin = state.origins.lock().unwrap().get(&id).unwrap().clone();
        assert!(origin.at.elapsed() < ORIGIN_RETENTION);
        assert_eq!(origin.owner, "original-owner");
        // A replay on another subscription cannot reassign the originating owner.
        state.observe("other/gpt", "other-owner", &event(&cipher));
        assert_eq!(state.origins.lock().unwrap()[&id].owner, "original-owner");
        state.origins.lock().unwrap().get_mut(&id).unwrap().at =
            Instant::now() - ORIGIN_RETENTION - Duration::from_secs(1);
        state.observe(
            "original/gpt",
            "original-owner",
            &event(&format!("gAAAA{}", "C".repeat(100))),
        );
        assert!(!state.origins.lock().unwrap().contains_key(&id));
        for number in 0..4100 {
            state.observe(
                "original/gpt",
                "original-owner",
                &event(&format!("gAAAA{}{number}", "D".repeat(100))),
            );
        }
        assert_eq!(state.origins.lock().unwrap().len(), 4096);
    }
}
