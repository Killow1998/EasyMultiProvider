//! Configuration commands used by management views.
use super::ChangeError;
use crate::app::ServerState;
use crate::services::catalog::{catalog_sources, refresh_catalog};
use emp_state::{canonicalize_private_paths, merge_web_update};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn update(state: &ServerState, incoming: &Value) -> Result<(), ChangeError> {
    let mut current = state
        .backend
        .configuration
        .edit()
        .map_err(|_| ChangeError::Unavailable)?;
    let mut updated = merge_web_update(&current, incoming).map_err(ChangeError::invalid)?;
    canonicalize_private_paths(&mut updated, &state.backend.configuration.config_path)
        .map_err(ChangeError::invalid)?;
    let sources = catalog_sources(state, &updated);
    let updated = sources
        .validate_update(&updated, &current)
        .map_err(ChangeError::invalid)?;
    let before = current.clone();
    current
        .commit(&updated)
        .map_err(|_| ChangeError::Unavailable)?;
    for id in changed_account_ids(&before, &current) {
        state.catalog_refresh.account_changed(&id);
    }
    drop(current);
    crate::services::account_catalog::request_refresh(state, false);
    // The usage worker applies changed pricing aliases and re-prices old rows.
    let _ = state.backend.usage.queue_scan(state);
    crate::services::runtime::mark_active_pending(state, "EMP configuration changed");
    Ok(())
}

pub(crate) fn context_preference(
    state: &ServerState,
    incoming: &Value,
) -> Result<bool, ChangeError> {
    let object = incoming
        .as_object()
        .filter(|object| object.len() == 1)
        .ok_or_else(|| {
            ChangeError::invalid(
                "catalog preference request must contain only catalog_show_context",
            )
        })?;
    let show_context = object
        .get("catalog_show_context")
        .and_then(Value::as_bool)
        .ok_or_else(|| ChangeError::invalid("catalog_show_context must be boolean"))?;
    let mut current = state
        .backend
        .configuration
        .edit()
        .map_err(|_| ChangeError::Unavailable)?;
    let mut updated = current.clone();
    updated["catalog_show_context"] = Value::Bool(show_context);
    current
        .commit(&updated)
        .map_err(|_| ChangeError::Unavailable)?;
    drop(current);
    let refreshed = refresh_catalog(state);
    crate::services::account_catalog::request_refresh(state, false);
    refreshed.map_err(|_| ChangeError::Unavailable)?;
    Ok(show_context)
}

fn changed_account_ids(before: &Value, after: &Value) -> BTreeSet<String> {
    fn source_identities(config: &Value) -> BTreeMap<String, Vec<(String, bool, String)>> {
        let mut accounts_by_id = BTreeMap::<String, Vec<(String, bool, String)>>::new();
        for account in config
            .get("accounts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = account.get("id").and_then(Value::as_str) {
                accounts_by_id.entry(id.to_owned()).or_default().push((
                    account
                        .get("auth_file")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    account.get("enabled") != Some(&Value::Bool(false)),
                    account
                        .get("credential_status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                ));
            }
        }
        accounts_by_id
    }

    let before = source_identities(before);
    let after = source_identities(after);
    before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|id| before.get(id) != after.get(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::changed_account_ids;
    use serde_json::json;

    #[test]
    fn configuration_account_changes_return_only_affected_ids() {
        let before = json!({
            "accounts":[
                {"id":"one","prefix":"old","name":"First","hidden_models":[],"auth_file":"/accounts/one/auth.json.enc","enabled":true,"credential_status":"valid"},
                {"id":"two","prefix":"same","auth_file":"/accounts/two/auth.json.enc","enabled":true}
            ]
        });
        let presentation_changed = json!({
            "accounts":[
                {"id":"one","prefix":"new","name":"Renamed","hidden_models":["model"],"auth_file":"/accounts/one/auth.json.enc","enabled":true,"credential_status":"valid"},
                {"id":"two","prefix":"same","auth_file":"/accounts/two/auth.json.enc","enabled":true,"credential_status":"unknown"}
            ]
        });
        assert!(changed_account_ids(&before, &presentation_changed).is_empty());

        let source_changed = json!({
            "accounts":[
                {"id":"one","prefix":"new","name":"Renamed","auth_file":"/accounts/one/auth.json.enc","enabled":false,"credential_status":"valid"},
                {"id":"three","prefix":"added","auth_file":"/accounts/three/auth.json.enc","enabled":true,"credential_status":"valid"}
            ]
        });
        assert_eq!(
            changed_account_ids(&before, &source_changed),
            std::collections::BTreeSet::from([
                "one".to_owned(),
                "three".to_owned(),
                "two".to_owned()
            ])
        );
    }
}
