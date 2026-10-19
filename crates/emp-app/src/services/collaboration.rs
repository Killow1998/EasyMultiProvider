//! Cross-service tasks: policy, original credential ownership, transient recovery.
use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::history::DestinationPrepareError;
use emp_core::{Dialect, ResolvedRoute};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

mod message;
mod recovery;
#[cfg(test)]
mod tests;

const RETENTION: Duration = Duration::from_secs(3600);
// Fingerprints and route ownership are not recovered plaintext. Keep them
// longer so ordinary long-running parent chats can still switch providers.
const ORIGIN_RETENTION: Duration = Duration::from_secs(24 * 3600);
const MAX_RECOVERIES: usize = 8;
const MAX_TEXT: usize = 256 * 1024;
type Key = [u8; 32];

#[derive(Clone)]
struct Origin {
    model: String,
    owner: String,
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

fn error(status: u16, code: &'static str, message: &'static str) -> DestinationPrepareError {
    DestinationPrepareError::Collaboration {
        status,
        code,
        message,
    }
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
}

pub(crate) fn prepare(
    state: &ServerState,
    route: &ResolvedRoute,
    mut body: Value,
    incoming: &BTreeMap<String, String>,
    mut monitor: Option<&mut DisconnectMonitor>,
) -> Result<Value, DestinationPrepareError> {
    if route.dialect == Dialect::CodexNative {
        return Ok(body);
    }
    let config = state.backend.configuration.read().map_err(|_| {
        error(
            503,
            "collaboration_config_unavailable",
            "Subagent settings are unavailable",
        )
    })?;
    let mode = config["external_collaboration"].as_str().unwrap_or("off");
    let child = message::is_child(&body, incoming);
    if mode == "off" && child {
        return Err(error(
            403,
            "external_subagents_disabled",
            "External subagents are disabled. Use a Native GPT subagent, or open EMP Settings > Cross-service subagents and select On-demand recovery or On. 设置 > 跨服务子代理：选择按需恢复或打开。",
        ));
    }
    if !body["input"]
        .as_array()
        .is_some_and(|items| items.iter().any(message::has_encrypted_task))
    {
        return Ok(body);
    }
    // Root-history recovery has its own opt-in. It must not turn on external
    // children or charge other users merely because they switch models.
    let root_request = !child
        && !incoming.iter().any(|(name, value)| {
            !value.is_empty()
                && (name.eq_ignore_ascii_case("x-openai-subagent")
                    || name.eq_ignore_ascii_case("x-codex-parent-thread-id"))
        });
    let history_recovery = root_request && config["encrypted_history_recovery"] == true;
    if mode != "on_demand" && !history_recovery {
        return Err(error(
            409,
            "encrypted_agent_task_requires_recovery",
            "This request contains encrypted agent messages. For an existing parent chat, enable Settings > Recover encrypted history (extra GPT subscription usage), or start a new external chat with a readable handoff. For an external child, select Cross-service subagents > On-demand recovery.",
        ));
    }
    // Snapshot only recovery work; ordinary requests need no configuration copy.
    let recovery_config = config.clone();
    drop(config);
    let items = body["input"].as_array().unwrap();
    if items
        .iter()
        .filter(|item| message::has_encrypted_task(item))
        .count()
        > MAX_RECOVERIES
    {
        return Err(error(
            413,
            "agent_task_recovery_limit",
            "Too many encrypted messages for one bounded recovery (maximum 8). Create a readable handoff with the original Native GPT model and continue in a new external chat.",
        ));
    }
    // Validate all envelopes and owners before spending quota on any of them.
    // A mixed-account/expired entry later in the input must not charge for a
    // partial recovery which can never be forwarded.
    let mut pending = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if !message::has_encrypted_task(item) {
            continue;
        }
        let task = message::parse(item)?;
        let service = &state.backend.transport.collaboration;
        let origin = service.origins.lock().unwrap().get(&key(task.ciphertext.as_bytes()))
            .filter(|origin| origin.at.elapsed() < ORIGIN_RETENTION).cloned()
            .ok_or_else(|| error(409, "agent_task_origin_unknown",
                "EMP no longer knows this message's original subscription (restart, expiry or eviction). Recovery cannot safely guess an account. For an existing chat, create a readable handoff with the original Native GPT model and continue in a new external chat; for a child, send a new task through EMP."))?;
        let (recovery_route, headers) =
            recovery::resolve(state, &recovery_config, &origin, incoming)?;
        let cache_key = key(&serde_json::to_vec(&json!([
            origin.owner,
            recovery_route.endpoint_fingerprint,
            item
        ]))
        .expect("task JSON"));
        pending.push((index, task, recovery_route, headers, cache_key));
    }
    for (index, task, recovery_route, headers, cache_key) in pending {
        let item = &mut body["input"][index];
        let service = &state.backend.transport.collaboration;
        let mut cache = service.recovered.lock().unwrap();
        cache.retain(|entry| entry.at.elapsed() < RETENTION);
        let text = if let Some(entry) = cache.iter().find(|entry| entry.key == cache_key) {
            entry.text.clone()
        } else {
            let text = recovery::execute(
                state,
                &recovery_route,
                &headers,
                item,
                monitor.as_deref_mut(),
            )?;
            if cache.len() == 64 {
                cache.pop_front();
            }
            cache.push_back(Recovered {
                key: cache_key,
                text: text.clone(),
                at: Instant::now(),
            });
            text
        };
        *item = json!({"type":"agent_message", "author":task.author, "recipient":task.recipient,
            "content":[{"type":"input_text", "text":format!("{}{text}", task.header)}]});
    }
    Ok(body)
}
