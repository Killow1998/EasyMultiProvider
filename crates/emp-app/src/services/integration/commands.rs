//! Complete integration commands. Wire adapters only authenticate, decode and render.
use crate::app::ServerState;
use crate::services::integration::integration_summary_with_result;
use crate::services::observation::operation::{OperationObservation, observe};
use crate::services::runtime::sync_runtime;
use emp_integration::IntegrationResult;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) enum Body {
    Summary(Value),
    Unavailable,
    NotFound,
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) body: Body,
}

pub(crate) fn read(state: &ServerState) -> Reply {
    summary(state, None, 200, None, None)
}

pub(crate) fn execute(
    request_id: Option<&str>,
    state: &ServerState,
    operation: &str,
    confirmed: bool,
    management_connections: usize,
) -> Reply {
    let name = match operation {
        "enable" => "integration_enable",
        "restore" => "integration_restore",
        "reload" => "integration_reload",
        "verify" => "integration_verify",
        _ => "integration_unknown",
    };
    let reply = observe(&state.backend.diagnostics, request_id, name, |receipt| {
        receipt.check("runtime_catalog_matches_target", None);
        receipt.check("request_routing_verified", None);
        receipt.check("desktop_effect_verified", None);
        let reply = execute_inner(state, operation, confirmed, management_connections, receipt);
        if let Body::Summary(value) = &reply.body {
            let expected = match operation {
                "enable" | "verify" => Some("emp_applied"),
                "restore" => Some("native"),
                _ => None,
            };
            receipt.check(
                "saved_configuration_matches_target",
                expected.and_then(|expected| {
                    value["configuration"]["state"]
                        .as_str()
                        .map(|actual| actual == expected)
                }),
            );
        }
        if reply.status < 400 {
            Ok(reply)
        } else {
            Err(reply)
        }
    })
    .unwrap_or_else(|reply| reply);
    // A failed follow-up can still leave a committed configuration change.
    // Every entry point invalidates the same WebUI snapshot after completion.
    if matches!(operation, "enable" | "restore" | "reload" | "verify") {
        state
            .backend
            .management_events
            .publish(crate::services::management_events::Change::Integration);
    }
    reply
}

fn execute_inner(
    state: &ServerState,
    operation: &str,
    confirmed: bool,
    management_connections: usize,
    receipt: &mut OperationObservation,
) -> Reply {
    if matches!(operation, "enable" | "restore") && !confirmed {
        return summary(
            state,
            Some(receipt),
            409,
            None,
            Some(
                json!({"message":"Confirmation is required before changing Codex integration files"}),
            ),
        );
    }
    let _restore_admission = if operation == "restore" {
        match receipt.step("quiesce", || {
            state
                .connection_admission
                .quiesce(management_connections, Duration::from_secs(15))
                .ok_or(())
        }) {
            Ok(gate) => Some(gate),
            Err(()) => {
                return summary(
                    state,
                    Some(receipt),
                    409,
                    None,
                    Some(json!({
                        "code":"active_conversations",
                        "message":"Finish active Codex requests and WebSockets, then retry restore"
                    })),
                );
            }
        }
    } else {
        None
    };
    let manager = &state.backend.integration.manager;
    let _operation = match receipt.step("operation_lock", || manager.operation_lock()) {
        Ok(lock) => lock,
        Err(_) => return unavailable(409),
    };
    if state.shutdown.load(Ordering::Acquire) {
        return unavailable(503);
    }
    if matches!(operation, "reload" | "verify") {
        let reload_target = if operation == "reload" {
            let status = match receipt.step("read_configuration", || manager.status()) {
                Ok(status) => status,
                Err(_) => return unavailable(409),
            };
            match (status.state.as_str(), status.relation.as_str()) {
                ("active", "applied") => Some("emp"),
                ("native" | "restored", "unleased" | "original") => Some("native"),
                _ => {
                    return summary(
                        state,
                        Some(receipt),
                        409,
                        None,
                        Some(
                            json!({"message":"Codex configuration is unresolved; resolve it before checking the runtime catalog"}),
                        ),
                    );
                }
            }
        } else {
            None
        };
        if operation != "reload" {
            let status = match receipt.step("read_configuration", || manager.status()) {
                Ok(status) => status,
                Err(_) => return unavailable(409),
            };
            if status.relation != "applied" {
                return summary(
                    state,
                    Some(receipt),
                    409,
                    None,
                    Some(
                        json!({"message":"EMP configuration is not applied; there is no EMP runtime to verify"}),
                    ),
                );
            }
        }
        drop(_operation);
        let result = match receipt.step("observe_runtime", || {
            sync_runtime(
                state,
                reload_target,
                operation == "reload" || confirmed,
                operation == "verify",
            )
        }) {
            Ok(result) => result,
            Err(_) => return unavailable(409),
        };
        record_runtime_result(receipt, &result);
        let successful = operation == "verify"
            || matches!(
                result.state,
                "catalog_unverified"
                    | "emp_loaded"
                    | "native_loaded"
                    | "reload_required"
                    | "stopped_waiting_for_start"
            );
        let error = (!successful).then(|| json!({"message":result.detail}));
        return summary(
            state,
            Some(receipt),
            if successful { 200 } else { 409 },
            None,
            error,
        );
    }
    let result = match operation {
        "enable" => match receipt.step("apply_configuration", || {
            crate::services::integration::enable::apply(state)
        }) {
            Ok(result) => result,
            Err(crate::services::integration::enable::EnableError::Unavailable(status)) => {
                return unavailable(status);
            }
            Err(crate::services::integration::enable::EnableError::EmptyCatalog) => {
                return summary(
                    state,
                    Some(receipt),
                    409,
                    None,
                    Some(json!({
                        "code":"empty_emp_catalog",
                        "message":"Keep at least one native or additional model visible before applying EMP to Codex"
                    })),
                );
            }
        },
        "restore" => {
            match receipt.step("restore_configuration", || {
                state
                    .backend
                    .integration
                    .restore_native_configuration(manager)
            }) {
                Ok(result) => result,
                Err(reason) => {
                    return summary(
                        state,
                        Some(receipt),
                        409,
                        None,
                        Some(json!({
                            "code":if reason == "active_codex_writer" { reason } else { "native_configuration_restore_failed" },
                            "message":crate::services::integration::restore_error_message(reason),
                            "reason":reason
                        })),
                    );
                }
            }
        }
        _ => {
            return Reply {
                status: 404,
                body: Body::NotFound,
            };
        }
    };
    receipt.check("configuration_command_succeeded", Some(result.ok()));
    if result.ok() {
        // Record ownership before optional follow-up work can fail. If its
        // rollback fails too, exit must still know which route to restore.
        let active = result.state == "active";
        state
            .backend
            .integration
            .owned
            .store(active, Ordering::Release);
        if result.state == "active"
            && receipt
                .step("apply_search", || {
                    crate::services::integration::sync_features(state)
                })
                .is_err()
        {
            if receipt
                .step("rollback_configuration", || manager.restore())
                .is_ok_and(|result| result.ok())
            {
                let _ =
                    emp_integration::local_auth::restore(&state.backend.accounts.native_auth_path);
            }
            return unavailable(409);
        }
        if let Ok(mut conflicts) = state.backend.integration.startup_conflicts.lock() {
            conflicts.clear();
        }
        drop(_operation);
        let runtime_sync = receipt.step("observe_runtime", || {
            sync_runtime(
                state,
                Some(if active { "emp" } else { "native" }),
                confirmed,
                false,
            )
        });
        if let Ok(result) = &runtime_sync {
            record_runtime_result(receipt, result);
        }
        let runtime_sync_failed = runtime_sync.is_err();
        // A native restore is already committed here. Runtime observation is
        // secondary; keep EMP available and return the committed result.
        if runtime_sync_failed && operation != "restore" {
            return unavailable(409);
        }
    }
    summary(
        state,
        Some(receipt),
        if result.ok() { 200 } else { 409 },
        Some(&result),
        None,
    )
}

fn unavailable(status: u16) -> Reply {
    Reply {
        status,
        body: Body::Unavailable,
    }
}

fn summary(
    state: &ServerState,
    receipt: Option<&mut OperationObservation>,
    status: u16,
    result: Option<&IntegrationResult>,
    error: Option<Value>,
) -> Reply {
    let read = || integration_summary_with_result(state, result);
    let observed = if let Some(receipt) = receipt {
        receipt.step("read_summary", read)
    } else {
        read()
    };
    let mut value = match observed {
        Ok(value) => value,
        Err(_) => return unavailable(503),
    };
    if let Some(error) = error {
        value["error"] = error;
    }
    Reply {
        status,
        body: Body::Summary(value),
    }
}

/// Called only with this command's existing, confirmed runtime observation.
/// A previous live snapshot is not evidence that this command performed a check.
fn record_runtime_result(
    receipt: &mut OperationObservation,
    result: &emp_codex::runtime_probe::RuntimeSyncResult,
) {
    receipt.fact("runtime_evidence", "this_command");
    receipt.check(
        "runtime_catalog_matches_target",
        match result.state {
            "emp_loaded" if result.verified => Some(true),
            "native_loaded" => Some(true), // only absence of previous EMP model IDs
            "reload_required" => Some(false),
            _ => None,
        },
    );
}
