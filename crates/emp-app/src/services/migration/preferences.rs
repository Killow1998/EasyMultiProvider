//! Resolve only subscription preference conflicts before the import transaction.
use super::MigrationError;
use crate::app::ServerState;
use crate::services::accounts::{account_credentials, native_auth_document};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Resolution {
    pub payload: Value,
    pub conflicts: Vec<Value>,
    pub updates: BTreeMap<String, Value>,
}

fn preferences(account: &Value) -> Value {
    json!({"hidden_models":account["hidden_models"].as_array().cloned().unwrap_or_default(),
        "model_context_windows":account["model_context_windows"].as_object().cloned().unwrap_or_default()})
}

pub(super) fn resolve(
    state: &ServerState,
    current: &Value,
    payload: &Value,
    choices: &Value,
    require_resolved: bool,
) -> Result<Resolution, MigrationError> {
    let native = native_auth_document(&state.backend.accounts.native_auth_path)
        .filter(|auth| emp_state::validate_auth_json(auth).is_ok());
    let mut credentials = account_credentials(current, &state.backend.configuration.vault);
    if let Some(native) = native {
        credentials.insert(0, ("@native".into(), native));
    }
    let mut owners: BTreeMap<String, Value> = current["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|account| {
            account["id"]
                .as_str()
                .map(|id| (id.to_owned(), account.clone()))
        })
        .collect();
    owners.insert("@native".into(),json!({"name":"Native","hidden_models":current["native_hidden_models"],"model_context_windows":current["native_model_context_windows"]}));
    let mut result = Resolution {
        payload: payload.clone(),
        conflicts: Vec::new(),
        updates: BTreeMap::new(),
    };
    let records = result.payload["accounts"]
        .as_array_mut()
        .ok_or_else(|| MigrationError::invalid("Invalid account selection"))?;
    for record in records {
        let source = record["metadata"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let Some((owner, _)) = credentials
            .iter()
            .find(|(_, auth)| emp_state::same_account_auth(auth, &record["auth"]))
        else {
            if require_resolved
                && choices[&source]
                    .as_object()
                    .is_some_and(|choices| !choices.is_empty())
            {
                return Err(MigrationError::invalid(
                    "Account changed after preview; review the import again",
                ));
            }
            let owner = format!("incoming:{source}");
            credentials.push((owner.clone(), record["auth"].clone()));
            owners.insert(owner, record["metadata"].clone());
            continue;
        };
        let owner = owner.clone();
        let existing = owners.get(&owner).cloned().unwrap_or(Value::Null);
        let label = existing["name"].as_str().unwrap_or(&owner).to_owned();
        let existing = preferences(&existing);
        let incoming = preferences(&record["metadata"]);
        let mut selected = incoming.clone();
        let mut fields = serde_json::Map::new();
        for (field, kind) in [
            ("hidden_models", "visibility"),
            ("model_context_windows", "context"),
        ] {
            // Omitted settings (e.g. disabled context transfer) keep the destination.
            if record["metadata"].get(field).is_none() {
                selected[field] = existing[field].clone();
                continue;
            }
            let equivalent = if kind == "visibility" {
                let hidden = |value: &Value| {
                    value
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<BTreeSet<_>>()
                };
                hidden(&existing[field]) == hidden(&incoming[field])
            } else {
                existing[field] == incoming[field]
            };
            if equivalent {
                continue;
            }
            let choice = choices[&source][kind].as_str().unwrap_or_default();
            let resolved = matches!(choice, "current" | "incoming");
            if require_resolved && !resolved {
                return Err(MigrationError::invalid(
                    "Choose model visibility and context settings in the import preview",
                ));
            }
            selected[field] = if choice == "incoming" {
                incoming[field].clone()
            } else {
                existing[field].clone()
            };
            fields.insert(kind.into(),json!({"current":existing[field],"incoming":incoming[field],"choice":choice,"resolved":resolved}));
        }
        if !fields.is_empty() {
            result.conflicts.push(
                json!({"source_id":source,"target_id":owner,"target_name":label,"fields":fields}),
            );
        }
        record["metadata"]["hidden_models"] = selected["hidden_models"].clone();
        record["metadata"]["model_context_windows"] = selected["model_context_windows"].clone();
        // Update every verified reference, while retaining credentials and route IDs.
        for (peer, auth) in &credentials {
            if emp_state::same_account_auth(auth, &record["auth"]) {
                result.updates.insert(peer.clone(), selected.clone());
            }
        }
        owners.insert(owner,json!({"name":label,"hidden_models":selected["hidden_models"],"model_context_windows":selected["model_context_windows"]}));
    }
    // Multiple entries for one incoming identity commit the same chosen settings.
    for record in result.payload["accounts"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        if let Some(selected) = credentials
            .iter()
            .filter(|(_, auth)| emp_state::same_account_auth(auth, &record["auth"]))
            .find_map(|(id, _)| result.updates.get(id))
        {
            record["metadata"]["hidden_models"] = selected["hidden_models"].clone();
            record["metadata"]["model_context_windows"] = selected["model_context_windows"].clone();
        }
    }
    Ok(result)
}

pub(super) fn apply(config: &mut Value, updates: &BTreeMap<String, Value>) {
    if let Some(native) = updates.get("@native") {
        config["native_hidden_models"] = native["hidden_models"].clone();
        config["native_model_context_windows"] = native["model_context_windows"].clone();
    }
    for account in config["accounts"].as_array_mut().into_iter().flatten() {
        if let Some(preferences) = account["id"].as_str().and_then(|id| updates.get(id)) {
            account["hidden_models"] = preferences["hidden_models"].clone();
            account["model_context_windows"] = preferences["model_context_windows"].clone();
        }
    }
}
