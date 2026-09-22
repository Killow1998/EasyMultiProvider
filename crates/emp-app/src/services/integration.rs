//! Services integration.
use emp_integration::IntegrationManager;
use std::sync::atomic::AtomicBool;

use crate::app::ServerState;
use emp_integration::IntegrationStatus;
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

pub(crate) fn integration_summary(state: &ServerState) -> Result<Value, String> {
    let status = state
        .backend
        .integration
        .manager
        .status()
        .map_err(|error| error.to_string())?;
    Ok(integration_summary_from_status(state, &status))
}

fn integration_summary_from_status(state: &ServerState, status: &IntegrationStatus) -> Value {
    let configuration_state = match status.state.as_str() {
        "active" => "emp_applied",
        "native" | "restored" => "native",
        state => state,
    };
    let next_action = match status.state.as_str() {
        "prepared" | "restoring" | "conflict" => "restore",
        "active" => "none",
        "native" | "restored" => "enable default Codex",
        _ => "none",
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
        "runtime":{
            "state":"not_checked","target":"native","verified":false,
            "confidence":"not_checked","action_required":false,
            "detail":"Codex runtime has not been checked","last_known":Value::Null
        },
        "service_health":"ready",
        "next_action":next_action,
    })
}

pub(crate) struct IntegrationState {
    pub(crate) manager: IntegrationManager,
    pub(crate) owned: AtomicBool,
}

impl IntegrationState {
    /// Restore only the lease acquired by this running service.
    pub(crate) fn restore_owned(&self) -> Result<(), crate::error::AppError> {
        use std::sync::atomic::Ordering;
        if !self.owned.load(Ordering::Acquire) {
            return Ok(());
        }
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
