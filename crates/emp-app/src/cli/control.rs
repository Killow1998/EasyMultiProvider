//! Offline doctor/restore commands; they never start a server or model request.
use crate::http::auth::codex_auth_path;
use emp_integration::runtime::{RuntimeStore, offline_snapshot};
use emp_integration::{IntegrationManager, IntegrationResult, IntegrationStatus};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Control {
    restore: bool,
    state_dir: Option<PathBuf>,
    json: bool,
}

impl Control {
    pub(crate) fn parse(
        command: &str,
        mut args: impl Iterator<Item = String>,
    ) -> Result<Self, String> {
        let mut control = Self {
            restore: command == "restore",
            state_dir: None,
            json: false,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--state-dir" => {
                    control.state_dir = Some(PathBuf::from(
                        args.next().ok_or("--state-dir requires a value")?,
                    ))
                }
                "--json" => control.json = true,
                _ => return Err(format!("unknown {command} option: {arg}")),
            }
        }
        Ok(control)
    }

    pub(crate) fn run(self) -> Result<ExitCode, String> {
        let unavailable = || "EMP integration operation failed".to_owned();
        let auth = codex_auth_path();
        let home = auth.parent().unwrap_or_else(|| Path::new("."));
        let state_dir = emp_state::config::resolve_user_path(
            &self
                .state_dir
                .unwrap_or_else(|| home.join("easy-multi-provider/integration")),
        );
        let manager =
            IntegrationManager::new(home.join("config.toml"), state_dir.join("lease.json"), None)
                .map_err(|_| unavailable())?
                .with_lock_path(state_dir.join("lease.lock"));
        let store = RuntimeStore::new(state_dir.join("runtime.json"));
        if self.restore {
            let prior = store.load().map_err(|_| unavailable())?;
            let result = manager.restore().map_err(|_| unavailable())?;
            if result.ok() {
                store
                    .save(
                        "reload_required",
                        "native",
                        &result.relation,
                        prior
                            .as_ref()
                            .map_or(&[], |value| value.expected_models.as_slice()),
                        false,
                        "Native configuration was restored offline; runtime was not checked",
                    )
                    .map_err(|_| unavailable())?;
            }
            let record = store.load().map_err(|_| unavailable())?;
            let runtime = offline_snapshot(record.as_ref(), "offline");
            print_result(&result, &runtime, self.json);
            Ok(if result.ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        } else {
            let status = manager.status().map_err(|_| unavailable())?;
            let record = store.load().map_err(|_| unavailable())?;
            let runtime = offline_snapshot(record.as_ref(), "offline");
            print_status(&status, &runtime, self.json);
            Ok(
                if matches!(status.state.as_str(), "native" | "active" | "restored") {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                },
            )
        }
    }
}

fn next_action(state: &str) -> &'static str {
    match state {
        "prepared" | "restoring" => "run restore",
        "active" => "confirm service health or run restore",
        "conflict" => "manually inspect; EMP will not overwrite user changes",
        _ => "none",
    }
}

fn print_status(status: &IntegrationStatus, runtime: &Value, as_json: bool) {
    let lease = status
        .lease
        .as_ref()
        .map_or("none", |lease| lease.status.as_str());
    if as_json {
        println!(
            "{}",
            json!({
                "state":status.state, "relation":status.relation, "config_exists":status.config_exists,
                "lease_status":lease, "conflicts":status.conflicts, "service_health":"not_checked",
                "runtime":runtime, "next_action":next_action(&status.state)
            })
        );
    } else {
        let conflicts = if status.conflicts.is_empty() {
            "none".to_owned()
        } else {
            status.conflicts.join(",")
        };
        println!(
            "state: {}\nrelation: {}\nconfig: {}\nlease: {}\nconflicts: {}\nservice health: not_checked\nruntime state: {}\nruntime confidence: {}\nnext action: {}",
            status.state,
            status.relation,
            if status.config_exists {
                "present"
            } else {
                "absent"
            },
            lease,
            conflicts,
            runtime["state"].as_str().unwrap_or_default(),
            runtime["confidence"].as_str().unwrap_or_default(),
            next_action(&status.state)
        );
    }
}

fn print_result(result: &IntegrationResult, runtime: &Value, as_json: bool) {
    if as_json {
        println!(
            "{}",
            json!({
                "configuration":{"action":result.action, "state":result.state, "relation":result.relation,
                    "lease_status":result.lease.as_ref().map_or("none",|lease|lease.status.as_str()), "conflicts":result.conflicts},
                "runtime":runtime, "next_action":next_action(&result.state)
            })
        );
    } else {
        let conflicts = if result.conflicts.is_empty() {
            "none".to_owned()
        } else {
            result.conflicts.join(",")
        };
        println!(
            "action: {}\nstate: {}\nrelation: {}\nconflicts: {}\nruntime state: {}\nruntime confidence: {}",
            result.action,
            result.state,
            result.relation,
            conflicts,
            runtime["state"].as_str().unwrap_or_default(),
            runtime["confidence"].as_str().unwrap_or_default()
        );
    }
}
