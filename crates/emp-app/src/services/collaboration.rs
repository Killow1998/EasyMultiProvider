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
            origins.retain(|_, origin| origin.at.elapsed() < RETENTION);
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
    if mode == "off" && message::is_child(&body, incoming) {
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
    if mode != "on_demand" {
        return Err(error(
            409,
            "encrypted_agent_task_requires_recovery",
            "This task was created with native encryption. Select On-demand recovery, or enable cross-service subagents and send a new task.",
        ));
    }
    // Snapshot only recovery work; ordinary requests need no configuration copy.
    let recovery_config = config.clone();
    drop(config);
    for item in body["input"].as_array_mut().unwrap() {
        if !message::has_encrypted_task(item) {
            continue;
        }
        let task = message::parse(item)?;
        let service = &state.backend.transport.collaboration;
        let origin = service.origins.lock().unwrap().get(&key(task.ciphertext.as_bytes()))
            .filter(|origin| origin.at.elapsed() < RETENTION).cloned()
            .ok_or_else(|| error(409, "agent_task_origin_unknown",
                "The task's original subscription is unavailable. Send a new task through EMP so it can retain the correct account."))?;
        let (recovery_route, headers) =
            recovery::resolve(state, &recovery_config, &origin, incoming)?;
        let cache_key = key(&serde_json::to_vec(&json!([
            origin.owner,
            recovery_route.endpoint_fingerprint,
            item
        ]))
        .expect("task JSON"));
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
