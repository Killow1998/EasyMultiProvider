use emp_modules::gateway::{Caller, CreatePlan, RemovePlan, ReplacePlan, Request};
use emp_modules::{Command, GatewayIdentity};
use serde_json::Value;

pub fn handle(command: Command) -> Result<Value, &'static str> {
    let request = match command {
        Command::Gateway { request } => request,
        Command::GatewayIdentify { digest, candidates } => Request::Identify { digest, candidates },
        _ => return Err("invalid_module_command"),
    };
    match request {
        Request::Identify { digest, candidates } => {
            serde_json::to_value(identify(&digest, candidates))
        }
        Request::Create { name } => {
            let trimmed = name.trim();
            if trimmed.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control)
            {
                return Err("invalid_device_name");
            }
            serde_json::to_value(CreatePlan {
                name: trimmed.into(),
            })
        }
        Request::Replace { id, device_ids } => {
            if id.is_empty() {
                return Err("missing_device_id");
            }
            let index = device_ids
                .iter()
                .position(|candidate| candidate == &id)
                .ok_or("device_key_not_found")?;
            serde_json::to_value(ReplacePlan { index })
        }
        Request::Remove { id, device_ids } => {
            if id.is_empty() {
                return Err("missing_device_id");
            }
            let keep = device_ids
                .iter()
                .enumerate()
                .filter_map(|(index, candidate)| (candidate != &id).then_some(index))
                .collect();
            serde_json::to_value(RemovePlan { keep })
        }
    }
    .map_err(|_| "invalid_gateway_response")
}

fn identify(digest: &str, candidates: Vec<GatewayIdentity>) -> Option<Caller> {
    candidates
        .into_iter()
        .find(|candidate| candidate.digest == digest)
        .map(|candidate| Caller {
            id: candidate.id,
            name: candidate.name,
        })
}
