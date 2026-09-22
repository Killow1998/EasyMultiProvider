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
    let runtime_state = runtime["state"]
        .as_str()
        .unwrap_or("not_checked")
        .to_owned();
    let action_required = matches!(
        runtime_state.as_str(),
        "catalog_unverified"
            | "reload_required"
            | "stop_failed"
            | "verification_failed"
            | "unsupported"
    );
    runtime["action_required"] = Value::Bool(action_required);
    let next_action = match runtime_state.as_str() {
        "catalog_unverified" => "restart Codex clients safely and check model display",
        "reload_required" => "wait for shared backend owner restart",
        "stopped_waiting_for_start" => "wait for shared backend owner start",
        _ if action_required => "check shared Codex backend",
        _ => match status.state.as_str() {
            "prepared" | "restoring" | "conflict" => "restore",
            "active" => "none",
            "native" => "enable default Codex",
            _ => "none",
        },
    };
    serde_json::json!({
        "codex_compatibility":crate::services::runtime::compatibility_snapshot(state, false),
        "configuration":{
            "state":configuration_state,
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

pub(crate) struct IntegrationState {
    pub(crate) manager: IntegrationManager,
    pub(crate) owned: AtomicBool,
    pub(crate) runtime: RuntimeState,
    pub(crate) inventory: emp_codex::runtime_inventory::RuntimeInventory,
}

impl IntegrationState {
    pub(crate) fn new(
        manager: IntegrationManager,
        codex_home: std::path::PathBuf,
        codex_binary: &str,
    ) -> Self {
        let runtime = RuntimeState::new(manager.lease_path().with_file_name("runtime.json"));
        Self {
            manager,
            owned: AtomicBool::new(false),
            runtime,
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
        let result = self
            .manager
            .restore()
            .map_err(|_| crate::error::AppError::ServerStopped)?;
        if !result.ok() {
            return Err(crate::error::AppError::ServerStopped);
        }
        self.owned.store(false, Ordering::Release);
        Ok(())
    }
}
