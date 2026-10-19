//! Cross-service tasks: policy, original credential ownership, transient recovery.
use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::history::DestinationPrepareError;
use emp_core::{Dialect, ResolvedRoute};
use serde_json::{Value, json};
use std::collections::BTreeMap;

mod message;
mod recovery;
mod state;
pub(crate) use state::CollaborationState;
use state::Origin;
#[cfg(test)]
mod tests;

const MAX_RECOVERIES: usize = 8;
const MAX_TEXT: usize = 256 * 1024;

fn error(status: u16, code: &'static str, message: &'static str) -> DestinationPrepareError {
    DestinationPrepareError::Collaboration {
        status,
        code,
        message,
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
    let child = message::is_child(incoming);
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
    // Existing On-demand recovery covers parent history and current child tasks.
    // Other worker kinds retain their separate policy. No session is rewritten.
    let other_worker = !child
        && incoming.iter().any(|(name, value)| {
            !value.is_empty()
                && (name.eq_ignore_ascii_case("x-openai-subagent")
                    || name.eq_ignore_ascii_case("x-codex-parent-thread-id"))
        });
    if other_worker {
        return Err(error(
            409,
            "encrypted_agent_task_requires_recovery",
            "Encrypted agent messages in review or compaction workers are outside cross-service recovery. Continue with the original Native model or a readable handoff. 审查或压缩工作线程中的加密子代理消息不适用跨服务按需恢复，请使用原 Native 模型或可读交接。",
        ));
    }
    if mode != "on_demand" {
        return Err(error(
            409,
            "encrypted_agent_task_requires_recovery",
            "This request contains encrypted agent messages. Select Settings > Cross-service subagents > On-demand recovery to transcribe them with GPT on the original account (extra latency and subscription usage), or continue with the original Native model or a readable handoff. 设置 > 跨服务子代理 > 按需恢复：使用原账号 GPT 转写加密子代理消息，会增加等待和订阅用量；也可继续使用原 Native 模型或可读交接。",
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
        let origin = service.origin(&task.ciphertext)
            .ok_or_else(|| error(409, "agent_task_origin_unknown",
                "The encrypted message's original subscription is unavailable (restart, expiry or eviction). Recovery cannot safely guess an account. Continue with the original Native model or prepare a readable handoff; for a child, send a new task through EMP."))?;
        let (recovery_route, headers) =
            recovery::resolve(state, &recovery_config, &origin, incoming)?;
        pending.push((index, task, recovery_route, headers));
    }
    for (index, task, recovery_route, headers) in pending {
        let item = &mut body["input"][index];
        let service = &state.backend.transport.collaboration;
        let text = service.recover(
            &emp_state::usage::account_owner(&headers),
            &recovery_route.endpoint_fingerprint,
            item,
            || {
                recovery::execute(
                    state,
                    &recovery_route,
                    &headers,
                    item,
                    monitor.as_deref_mut(),
                )
            },
        )?;
        *item = json!({"type":"agent_message", "author":task.author, "recipient":task.recipient,
            "content":[{"type":"input_text", "text":format!("{}{text}", task.header)}]});
    }
    Ok(body)
}
