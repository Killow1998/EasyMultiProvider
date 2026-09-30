//! Services integration.
use emp_integration::IntegrationManager;
use std::sync::atomic::AtomicBool;

use crate::app::ServerState;
use crate::services::runtime::RuntimeState;
use emp_integration::{IntegrationResult, IntegrationStatus};
use serde_json::Value;

pub(crate) fn integration_summary_with_result(
    state: &ServerState,
    result: Option<&IntegrationResult>,
) -> Result<Value, String> {
    let mut status = state
        .backend
        .integration
        .manager
        .status()
        .map_err(|error| error.to_string())?;
    let conflicts = state
        .backend
        .integration
        .startup_conflicts
        .lock()
        .map_err(|_| "integration state is unavailable")?
        .clone();
    if !conflicts.is_empty() {
        status.state = "conflict".to_owned();
        status.conflicts = conflicts;
    }
    if let Some(result) = result.filter(|result| !result.ok()) {
        status.state = "conflict".to_owned();
        status.relation = result.relation.clone();
        status.conflicts = result.conflicts.clone();
    }
    Ok(integration_summary_from_status(state, &status))
}

fn integration_summary_from_status(state: &ServerState, status: &IntegrationStatus) -> Value {
    let configuration_state = match status.state.as_str() {
        "active" => "emp_applied",
        "native" | "restored" => "native",
        state => state,
    };
    let mut runtime = state.backend.integration.runtime.snapshot();
    let saved_target = match configuration_state {
        "emp_applied" => Some("emp"),
        "native" => Some("native"),
        _ => None,
    };
    let configuration_next_action = suppress_unsettled_runtime(&mut runtime, configuration_state);
    let configuration_unsettled = configuration_next_action.is_some();
    if !configuration_unsettled && let Some(saved_target) = saved_target {
        let recorded_target = runtime["target"].as_str().map(str::to_owned);
        if recorded_target.as_deref() != Some(saved_target) {
            mark_runtime_unchecked(
                &mut runtime,
                "The saved configuration target differs from the last runtime observation; the current target has not been checked",
            );
            runtime["target"] = Value::String(saved_target.to_owned());
        }
        runtime["saved_target"] = Value::String(saved_target.to_owned());
        runtime["target_matches_saved_configuration"] =
            Value::Bool(recorded_target.as_deref() == Some(saved_target));
    }
    let runtime_state = runtime["state"]
        .as_str()
        .unwrap_or("not_checked")
        .to_owned();
    if runtime["last_known"].is_object() {
        let last_state = runtime["last_known"]["state"]
            .as_str()
            .unwrap_or("not_checked");
        let public_state = public_probe_state(last_state, runtime["last_known"]["target"].as_str());
        if public_state != last_state {
            runtime["last_known"]["state"] = Value::String(public_state.to_owned());
        }
    }
    let runtime_state = public_probe_state(&runtime_state, runtime["target"].as_str()).to_owned();
    runtime["state"] = Value::String(runtime_state.clone());
    let confidence = runtime["confidence"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let catalog_observation = confidence == "live"
        && matches!(
            runtime_state.as_str(),
            "catalog_loaded"
                | "emp_catalog_absent"
                | "reload_required"
                | "catalog_unverified"
                | "verification_failed"
        );
    let catalog_verified = catalog_observation
        && runtime["verified"].as_bool() == Some(true)
        && runtime_state == "catalog_loaded";
    if runtime["last_known"].is_object() {
        let last_state = runtime["last_known"]["state"]
            .as_str()
            .unwrap_or("not_checked")
            .to_owned();
        let last_catalog_verified = runtime["last_known"]["verified"].as_bool() == Some(true)
            && last_state == "catalog_loaded";
        let last_scope = match last_state.as_str() {
            "catalog_loaded" => "emp_model_catalog",
            "emp_catalog_absent" => "emp_model_absence",
            _ => "none",
        };
        runtime["last_known"]["verification_scope"] = Value::String(last_scope.to_owned());
        runtime["last_known"]["catalog_verified"] = Value::Bool(last_catalog_verified);
        runtime["last_known"]["emp_models_absent"] =
            Value::Bool(last_state == "emp_catalog_absent");
        runtime["last_known"]["routing_verified"] = Value::Bool(false);
        runtime["last_known"]["restoration_verified"] = Value::Bool(false);
    }
    let verification_scope = match runtime_state.as_str() {
        "catalog_loaded" => "emp_model_catalog",
        "emp_catalog_absent" => "emp_model_absence",
        "reload_required" | "catalog_unverified" | "verification_failed" if catalog_observation => {
            "model_catalog_probe"
        }
        _ => "none",
    };
    runtime["verification_scope"] = Value::String(verification_scope.to_owned());
    runtime["catalog_verified"] = Value::Bool(catalog_verified);
    runtime["emp_models_absent"] =
        Value::Bool(confidence == "live" && runtime_state == "emp_catalog_absent");
    runtime["routing_verified"] = Value::Bool(false);
    runtime["restoration_verified"] = Value::Bool(false);
    runtime["routing_state"] = Value::String("not_observable".to_owned());
    let action_required = configuration_unsettled
        || matches!(
            runtime_state.as_str(),
            "catalog_unverified"
                | "reload_required"
                | "stop_failed"
                | "verification_failed"
                | "unsupported"
        );
    runtime["action_required"] = Value::Bool(action_required);
    let next_action = if let Some(action) = configuration_next_action {
        action
    } else {
        match runtime_state.as_str() {
            "catalog_unverified" | "stopped_waiting_for_start" => {
                "The shared model catalog is not yet verified; check the Codex model picker when convenient"
            }
            "reload_required" if runtime["target"] == "native" => {
                "The saved native configuration differs from the running catalog; let active chats finish, then ask the shared backend owner to restart it normally"
            }
            "reload_required" => {
                "The saved EMP catalog differs from the running catalog; let active chats finish, then ask the shared backend owner to restart it normally"
            }
            "catalog_loaded" => {
                "The model catalog matches the saved target; request routing is not observable through the current control API"
            }
            "emp_catalog_absent" => {
                "The shared list has no previous EMP-specific model IDs; this does not verify restored native settings or request routing"
            }
            "not_checked" if runtime["target_matches_saved_configuration"] == false => {
                "The last runtime observation targeted previous configuration; the current saved target has not been checked"
            }
            "not_checked" => "No live model catalog check has run in this EMP process",
            _ if action_required => "check shared Codex backend",
            _ => match status.state.as_str() {
                "prepared" | "restoring" | "conflict" => "restore",
                "active" => "none",
                "native" => "enable default Codex",
                _ => "none",
            },
        }
    };
    serde_json::json!({
        "codex_compatibility":crate::services::runtime::compatibility_snapshot(state, false),
        "configuration":{
            "state":configuration_state,
            "persisted":true,
            "source":"current_config_and_lease",
            "relation":status.relation,
            "config_exists":status.config_exists,
            "lease_status":status.lease.as_ref().map_or("none",|lease|lease.status.as_str()),
            "conflicts":status.conflicts,
        },
        "runtime":runtime,
        "service_health":"ready",
        "next_action":next_action,
    })
}

fn mark_runtime_unchecked(runtime: &mut Value, detail: &str) {
    if runtime["confidence"] == "live" {
        runtime["last_known"] = serde_json::json!({
            "state":runtime["state"],
            "target":runtime["target"],
            "verified":runtime["verified"],
            "confidence":"live",
            "detail":runtime["detail"]
        });
    }
    runtime["state"] = Value::String("not_checked".to_owned());
    runtime["verified"] = Value::Bool(false);
    runtime["confidence"] = Value::String("stale".to_owned());
    runtime["detail"] = Value::String(detail.to_owned());
}

fn public_probe_state<'a>(state: &'a str, target: Option<&str>) -> &'a str {
    match (state, target) {
        ("emp_loaded", Some("emp")) => "catalog_loaded",
        ("native_loaded", Some("native")) => "emp_catalog_absent",
        _ => state,
    }
}

fn suppress_unsettled_runtime(
    runtime: &mut Value,
    configuration_state: &str,
) -> Option<&'static str> {
    let next_action = match configuration_state {
        "emp_applied" | "native" => return None,
        "conflict" => "resolve the Codex configuration conflict before checking the model catalog",
        "prepared" | "restoring" => {
            "finish or restore the pending Codex configuration before checking the model catalog"
        }
        _ => "confirm the saved Codex configuration before checking the model catalog",
    };
    mark_runtime_unchecked(
        runtime,
        "Codex configuration is unresolved; the runtime catalog cannot be compared with a saved target",
    );
    runtime["target"] = Value::Null;
    runtime["target_matches_saved_configuration"] = Value::Null;
    Some(next_action)
}

pub(crate) struct IntegrationState {
    pub(crate) manager: IntegrationManager,
    pub(crate) search: emp_integration::search::SearchFeatureManager,
    pub(crate) owned: AtomicBool,
    pub(crate) startup_conflicts: std::sync::Mutex<Vec<String>>,
    pub(crate) runtime: RuntimeState,
    pub(crate) watch: crate::services::runtime::RuntimeWatch,
    pub(crate) inventory: emp_codex::runtime_inventory::RuntimeInventory,
}

/// Run the portable-history guard and restore Codex configuration as one
/// operation. The caller holds `IntegrationManager::operation_lock()` so CLI,
/// API, and lifecycle restoration use the same ordering.
pub(crate) fn restore_native_with_history(
    manager: &IntegrationManager,
    search: Option<&emp_integration::search::SearchFeatureManager>,
) -> Result<
    (
        IntegrationResult,
        emp_codex::history::repair::HistoryRepairReport,
    ),
    &'static str,
> {
    let home = manager
        .config_path()
        .parent()
        .ok_or("codex_home_unavailable")?;
    let (history_lock, report) = emp_codex::history::repair::repair_before_native_restore(home)
        .map_err(|error| error.reason())?;
    if let Some(search) = search {
        search
            .restore()
            .map_err(|_| "search_configuration_restore_failed")?;
    }
    let result = manager
        .restore()
        .map_err(|_| "native_configuration_restore_failed")?;
    drop(history_lock);
    Ok((result, report))
}

pub(crate) fn restore_error_message(reason: &str) -> &'static str {
    match reason {
        "active_codex_writer" => {
            "Conversation history is still open in Codex. Close the ChatGPT/Codex app, CLI sessions and Codex IDE sessions, then retry. EMP is still running."
        }
        _ => {
            "Native settings were left unchanged because conversation history could not be made portable"
        }
    }
}

impl IntegrationState {
    pub(crate) fn new(
        manager: IntegrationManager,
        codex_home: std::path::PathBuf,
        codex_binary: &str,
    ) -> Self {
        let runtime = RuntimeState::new(manager.lease_path().with_file_name("runtime.json"));
        Self {
            search: emp_integration::search::SearchFeatureManager::new(
                manager.config_path().to_owned(),
                manager.lease_path().with_file_name("search.json"),
            ),
            manager,
            owned: AtomicBool::new(false),
            startup_conflicts: std::sync::Mutex::new(Vec::new()),
            runtime,
            watch: Default::default(),
            inventory: emp_codex::runtime_inventory::RuntimeInventory::new(
                codex_home,
                (codex_binary != "codex").then(|| codex_binary.into()),
            ),
        }
    }

    /// Restore only the lease acquired by this running service.
    pub(crate) fn restore_owned(&self) -> Result<(), crate::error::AppError> {
        use std::sync::atomic::Ordering;
        if !self.owned.load(Ordering::Acquire) {
            return Ok(());
        }
        let _operation = self
            .manager
            .operation_lock()
            .map_err(|_| crate::error::AppError::ServerStopped)?;
        let (result, _) =
            restore_native_with_history(&self.manager, Some(&self.search)).map_err(|reason| {
                eprintln!("EMP native restore stopped: {reason}");
                crate::error::AppError::NativeRestoreBlocked(reason)
            })?;
        if !result.ok() {
            return Err(crate::error::AppError::ServerStopped);
        }
        self.owned.store(false, Ordering::Release);
        Ok(())
    }
}

pub(crate) fn sync_search(state: &ServerState) -> Result<(), ()> {
    let enabled = state.backend.configuration.config.lock().map_err(|_| ())?["subscription_search"]
        ["enabled"]
        == true;
    state
        .backend
        .integration
        .search
        .apply(enabled)
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::suppress_unsettled_runtime;
    use serde_json::json;

    #[test]
    fn unresolved_configuration_hides_catalog_match_and_keeps_last_observation() {
        for configuration_state in ["conflict", "prepared", "restoring", "unsupported"] {
            let mut runtime = json!({
                "state":"emp_loaded",
                "target":"emp",
                "verified":true,
                "confidence":"live",
                "detail":"catalog matched",
                "last_known":null
            });

            let next_action =
                suppress_unsettled_runtime(&mut runtime, configuration_state).unwrap();
            match configuration_state {
                "conflict" => assert!(next_action.contains("configuration conflict")),
                "prepared" | "restoring" => {
                    assert!(next_action.contains("pending Codex configuration"))
                }
                _ => assert!(next_action.contains("confirm the saved Codex configuration")),
            }
            assert_eq!(runtime["state"], "not_checked");
            assert_eq!(runtime["verified"], false);
            assert_eq!(runtime["confidence"], "stale");
            assert!(runtime["target"].is_null());
            assert!(runtime["target_matches_saved_configuration"].is_null());
            assert_eq!(runtime["last_known"]["state"], "emp_loaded");
            assert_eq!(runtime["last_known"]["target"], "emp");
            assert_eq!(runtime["last_known"]["verified"], true);
        }
    }
}
