//! Services integration.
use emp_integration::IntegrationManager;
use std::sync::atomic::AtomicBool;

use crate::app::ServerState;
use crate::services::runtime::RuntimeState;
use emp_integration::{IntegrationResult, IntegrationStatus};
use serde_json::Value;

fn codex_compatibility(state: &ServerState) -> Value {
    let installed = std::process::Command::new(&state.backend.accounts.codex_binary)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let text = String::from_utf8_lossy(&output.stdout);
            text.split_whitespace()
                .find(|part| {
                    part.trim_start_matches('v')
                        .split('.')
                        .take(3)
                        .all(|value| value.chars().all(|character| character.is_ascii_digit()))
                        && part.matches('.').count() >= 2
                })
                .map(|value| value.trim_start_matches('v').to_owned())
        });
    let status = installed.as_deref().map_or("unavailable", |version| {
        let mut parts = version
            .split(['.', '-', '+'])
            .take(3)
            .filter_map(|value| value.parse::<u64>().ok());
        match (parts.next(), parts.next(), parts.next()) {
            (Some(0), Some(155), Some(_)) if !version.contains('-') => "recommended",
            (Some(0), Some(149..=154), Some(_)) if !version.contains('-') => "supported",
            (Some(major), Some(minor), Some(_)) if (major, minor) < (0, 149) => "unsupported",
            _ => "unverified",
        }
    });
    serde_json::json!({
        "installed":installed,
        "status":status,
        "supported_range":"0.149.x–0.155.x",
        "recommended":"0.155.0"
    })
}

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
        "codex_compatibility":codex_compatibility(state),
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
}

impl IntegrationState {
    pub(crate) fn new(manager: IntegrationManager) -> Self {
        let runtime = RuntimeState::new(manager.lease_path().with_file_name("runtime.json"));
        Self {
            manager,
            owned: AtomicBool::new(false),
            runtime,
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
