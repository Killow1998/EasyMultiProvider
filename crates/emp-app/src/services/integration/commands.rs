//! Complete integration commands. Wire adapters only authenticate, decode and render.
use crate::app::ServerState;
use crate::services::integration::{integration_summary_with_result, restore_native_with_history};
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
    pub(crate) stop_after_write: bool,
}

pub(crate) fn read(state: &ServerState) -> Reply {
    summary(state, None, 200, None, None)
}

pub(crate) fn execute(
    request_id: Option<&str>,
    state: &ServerState,
    operation: &str,
    confirmed: bool,
) -> Reply {
    let name = match operation {
        "enable" => "integration_enable",
        "restore" => "integration_restore",
        "reload" => "integration_reload",
        "verify" => "integration_verify",
        _ => "integration_unknown",
    };
    observe(&state.backend.diagnostics, request_id, name, |receipt| {
        receipt.check("runtime_catalog_matches_target", None);
        receipt.check("request_routing_verified", None);
        receipt.check("desktop_effect_verified", None);
        let reply = execute_inner(state, operation, confirmed, receipt);
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
    .unwrap_or_else(|reply| reply)
}

fn execute_inner(
    state: &ServerState,
    operation: &str,
    confirmed: bool,
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
    let mut restore_admission = if operation == "restore" {
        match receipt.step("quiesce", || {
            state
                .connection_admission
                .quiesce(1, Duration::from_secs(15))
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
        let result = match receipt.step("observe_runtime", || {
            sync_runtime(
                state,
                reload_target,
                operation == "reload" || confirmed,
                operation == "verify",
                false,
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
            match receipt.step("restore_configuration_and_history", || {
                restore_native_with_history(manager, Some(&state.backend.integration.search))
            }) {
                Ok((result, _)) => result,
                Err(reason) => {
                    return summary(
                        state,
                        Some(receipt),
                        409,
                        None,
                        Some(json!({
                            "code":if reason == "active_codex_writer" { reason } else { "native_history_restore_blocked" },
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
                stop_after_write: false,
            };
        }
    };
    let mut stop_after_restore_response = false;
    receipt.check("configuration_command_succeeded", Some(result.ok()));
    if result.ok() {
        if result.state == "active"
            && receipt
                .step("apply_search", || {
                    crate::services::integration::sync_search(state)
                })
                .is_err()
        {
            let _ = receipt.step("rollback_configuration", || manager.restore());
            return unavailable(409);
        }
        if let Ok(mut conflicts) = state.backend.integration.startup_conflicts.lock() {
            conflicts.clear();
        }
        let active = result.state == "active";
        state
            .backend
            .integration
            .owned
            .store(active, Ordering::Release);
        stop_after_restore_response = operation == "restore" && !active;
        let runtime_sync = receipt.step("observe_runtime", || {
            sync_runtime(
                state,
                Some(if active { "emp" } else { "native" }),
                confirmed,
                false,
                false,
            )
        });
        if let Ok(result) = &runtime_sync {
            record_runtime_result(receipt, result);
        }
        let runtime_sync_failed = runtime_sync.is_err();
        // A native restore is already committed here. Runtime observation is
        // secondary; return the restore result before stopping EMP, and leave
        // the service available if the final summary cannot be prepared.
        if runtime_sync_failed && !stop_after_restore_response {
            return unavailable(409);
        }
    }
    let mut response = summary(
        state,
        Some(receipt),
        if result.ok() { 200 } else { 409 },
        Some(&result),
        None,
    );
    if stop_after_restore_response && response.status == 200 {
        if let Some(gate) = restore_admission.take() {
            gate.keep_closed();
        }
        response.stop_after_write = true;
    }
    response
}

fn unavailable(status: u16) -> Reply {
    Reply {
        status,
        body: Body::Unavailable,
        stop_after_write: false,
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
        stop_after_write: false,
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
