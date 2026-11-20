use crate::app::ServerState;
use crate::services::history::DestinationPrepareError;
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn validate(
    state: &ServerState,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<(), DestinationPrepareError> {
    let config =
        state
            .backend
            .configuration
            .read()
            .map_err(|_| DestinationPrepareError::Collaboration {
                status: 503,
                code: "collaboration_config_unavailable",
                message: "Subagent settings are unavailable",
            })?;
    let child = match incoming
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("x-openai-subagent"))
    {
        Some((_, worker)) => worker == "collab_spawn",
        None => incoming.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-codex-parent-thread-id") && !value.is_empty()
        }),
    };
    if child && config["external_collaboration"] == "off" {
        return Err(DestinationPrepareError::Collaboration {
            status: 400,
            code: "external_subagents_disabled",
            message: "External subagents are disabled. Enable Settings > Cross-service subagents, or select a Native model. 跨服务子代理已关闭，可在 EMP 设置中打开或改用原生模型。",
        });
    }
    if body["input"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            item["type"] == "agent_message"
                && (item.get("encrypted_content").is_some()
                    || item["content"].as_array().is_some_and(|parts| {
                        parts.iter().any(|part| part["type"] == "encrypted_content")
                    }))
        })
    }) {
        return Err(DestinationPrepareError::Collaboration {
            status: 400,
            code: "encrypted_agent_task_requires_recovery",
            message: "Continue with the original Native model or create a readable handoff before switching services. 请用原生模型继续，或生成可读交接后再切换服务。",
        });
    }
    Ok(())
}
