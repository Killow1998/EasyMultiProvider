use super::{ChangeError, CommittedChange, ConfigurationEdit, after_commit};
use crate::app::ServerState;
use crate::services::catalog::validate_update;
use emp_state::{canonicalize_private_paths, config::merge_web_update_with_registry};
use serde_json::Value;

pub(crate) fn update(
    request_id: Option<&str>,
    state: &ServerState,
    incoming: &Value,
) -> Result<(), ChangeError> {
    crate::services::observation::operation::observe(
        &state.backend.diagnostics,
        request_id,
        "configuration_update",
        |receipt| {
            let _operation = state
                .backend
                .integration
                .manager
                .operation_lock()
                .map_err(|_| ChangeError::Unavailable)?;
            let mut current = state
                .backend
                .configuration
                .edit()
                .map_err(|_| ChangeError::Unavailable)?;
            let mut updated = merge_web_update_with_registry(
                &current,
                incoming,
                state.backend.configuration.official_models,
            )
            .map_err(ChangeError::invalid)?;
            emp_state::config::validate_service_names(&current, &updated)
                .map_err(ChangeError::invalid)?;
            canonicalize_private_paths(&mut updated, &state.backend.configuration.config_path)
                .map_err(ChangeError::invalid)?;
            validate_update(state, &updated, &current).map_err(ChangeError::invalid)?;
            if current["aero_profiles"] != updated["aero_profiles"] {
                if !state.backend.modules.active(emp_modules::Module::Aero) {
                    return Err(ChangeError::invalid("module_disabled"));
                }
                crate::services::modules::aero::validate_update(&current, &updated, || {
                    crate::services::catalog::source_catalog(state, &updated)
                })
                .map_err(ChangeError::invalid)?;
            }
            let before = current.clone();
            let preferences = &state.backend.integration.preferences;
            let preferences_changed = (before["background_compaction"]
                != updated["background_compaction"]
                || before["memory_model"] != updated["memory_model"]
                || before["memory_extraction_model"] != updated["memory_extraction_model"])
                && state
                    .backend
                    .integration
                    .owned
                    .load(std::sync::atomic::Ordering::Acquire);
            if preferences_changed {
                let applied = preferences.apply(
                    updated["background_compaction"].as_bool(),
                    updated["memory_extraction_model"]
                        .as_str()
                        .unwrap_or_default(),
                    updated["memory_model"].as_str().unwrap_or_default(),
                );
                if let Err(error) = applied {
                    let _ = preferences.rollback(
                        before["background_compaction"].as_bool(),
                        before["memory_extraction_model"]
                            .as_str()
                            .unwrap_or_default(),
                        before["memory_model"].as_str().unwrap_or_default(),
                    );
                    return Err(ChangeError::invalid(error));
                }
            }
            if receipt
                .step("commit_configuration", || current.commit(&updated))
                .is_err()
            {
                if preferences_changed {
                    let _ = preferences.rollback(
                        before["background_compaction"].as_bool(),
                        before["memory_extraction_model"]
                            .as_str()
                            .unwrap_or_default(),
                        before["memory_model"].as_str().unwrap_or_default(),
                    );
                }
                return Err(ChangeError::Unavailable);
            }
            receipt.check("configuration_committed", Some(true));
            let ConfigurationEdit { current, .. } = current;
            after_commit(
                state,
                CommittedChange::Settings {
                    before: &before,
                    current,
                    receipt,
                },
            )
            .map_err(|_| ChangeError::Unavailable)?;
            Ok(())
        },
    )
}

pub(crate) fn context_preference(
    request_id: Option<&str>,
    state: &ServerState,
    show_context: Result<bool, ChangeError>,
) -> Result<bool, ChangeError> {
    crate::services::observation::operation::observe(
        &state.backend.diagnostics,
        request_id,
        "catalog_preference",
        |receipt| {
            let show_context = show_context?;
            let mut current = state
                .backend
                .configuration
                .edit()
                .map_err(|_| ChangeError::Unavailable)?;
            let mut updated = current.clone();
            updated["catalog_show_context"] = Value::Bool(show_context);
            receipt
                .step("commit_configuration", || current.commit(&updated))
                .map_err(|_| ChangeError::Unavailable)?;
            receipt.check("configuration_committed", Some(true));
            receipt.check(
                "preference_matches_saved",
                current
                    .get("catalog_show_context")
                    .and_then(Value::as_bool)
                    .map(|actual| actual == show_context),
            );
            drop(current);
            after_commit(state, CommittedChange::ContextPreference(receipt))
                .map_err(|_| ChangeError::Unavailable)?;
            Ok(show_context)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::super::publication::changed_account_ids;
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
