//! Model discovery through the installed CLI's initialization control response.
use super::{local_query::with_login, process};
use crate::app::ServerState;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn discover(state: &ServerState) -> Result<Vec<Value>, &'static str> {
    with_login(state, false, |config, _, cancelled| {
        let input = b"{\"type\":\"control_request\",\"request_id\":\"emp-models\",\"request\":{\"subtype\":\"initialize\"}}\n";
        let stdout =
            process::run_control(process::control_command(config), input, cancelled, || {
                state
                    .shutdown
                    .load(std::sync::atomic::Ordering::Acquire)
                    .then_some(process::CancellationReason::ServerShutdown)
            })?;
        project(&stdout)
    })
}

fn project(stdout: &[u8]) -> Result<Vec<Value>, &'static str> {
    let reply = stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|event| {
            event["type"] == "control_response" && event["response"]["request_id"] == "emp-models"
        })
        .ok_or("claude_cli_models_unsupported")?;
    if reply["response"]["subtype"] != "success" {
        return Err("claude_cli_models_unsupported");
    }
    let rows = reply["response"]["response"]["models"]
        .as_array()
        .ok_or("claude_cli_models_unsupported")?;
    if rows.len() > emp_router::discovery::MAX_DISCOVERED_MODELS {
        return Err("claude_cli_model_list_too_large");
    }
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for row in rows {
        let id = row["value"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .ok_or("claude_cli_invalid_model_list")?;
        if !seen.insert(id) {
            continue;
        }
        // Only model fields enter the saved list; account info and other startup
        // output never reach the management UI or persisted cache.
        let mut model = json!({"upstream_id":id,"display_name":row["displayName"].as_str().unwrap_or(id),
            "supported_protocols":["anthropic_messages"],"capability_sources":{}});
        if let Some(effort) = row["supportsEffort"].as_bool() {
            model["supports_reasoning"] = effort.into();
            model["capability_sources"]["supports_reasoning"] = json!({"source":"advertised"});
        }
        if let Some(levels) = row["supportedEffortLevels"].as_array() {
            let levels: Vec<_> = levels
                .iter()
                .filter_map(Value::as_str)
                .filter(|level| matches!(*level, "low" | "medium" | "high" | "xhigh" | "max"))
                .collect();
            model["reasoning_levels"] = json!(levels);
            model["capability_sources"]["reasoning_levels"] = json!({"source":"advertised"});
        }
        models.push(model);
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_or_failed_initialize_never_becomes_an_empty_success() {
        for response in [
            json!({}),
            json!({"subtype":"error","error":"private detail"}),
            json!({"subtype":"success","response":{"account":{"email":"private"}}}),
            json!({"subtype":"success","response":{"models":[{"displayName":"missing ID"}]}}),
        ] {
            let event = json!({"type":"control_response","response":{"request_id":"emp-models"}});
            let mut event = event;
            for (key, value) in response.as_object().unwrap() {
                event["response"][key] = value.clone();
            }
            assert!(project(event.to_string().as_bytes()).is_err());
        }
    }
}
