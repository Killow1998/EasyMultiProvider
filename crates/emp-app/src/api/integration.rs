//! Integration mutations and read-only verification of the shared Codex runtime.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::integration::integration_summary_with_result;
use crate::services::integration::restore_native_with_history;
use crate::services::runtime::sync_runtime;
use emp_integration::IntegrationResult;
use serde_json::{Value, json};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) fn read_integration_request(state: &ServerState) -> Vec<u8> {
    summary_response(state, 200, None, None)
}

pub(crate) fn management_integration_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    stop_after_write: &mut bool,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_token().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let operation = request.raw_path().rsplit('/').next().unwrap_or_default();
    let confirmed = body.get("confirm_reload") == Some(&Value::Bool(true));
    if matches!(operation, "enable" | "restore") && !confirmed {
        return summary_response(
            state,
            409,
            None,
            Some(
                json!({"message":"Confirmation is required before changing Codex integration files"}),
            ),
        );
    }
    let mut restore_admission = if operation == "restore" {
        match state
            .connection_admission
            .quiesce(1, Duration::from_secs(15))
        {
            Some(gate) => Some(gate),
            None => {
                return summary_response(
                    state,
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
    let _operation = match manager.operation_lock() {
        Ok(lock) => lock,
        Err(_) => return unavailable(409),
    };
    if matches!(operation, "reload" | "verify") {
        let reload_target = if operation == "reload" {
            let status = match manager.status() {
                Ok(status) => status,
                Err(_) => return unavailable(409),
            };
            match (status.state.as_str(), status.relation.as_str()) {
                ("active", "applied") => Some("emp"),
                ("native" | "restored", "unleased" | "original") => Some("native"),
                _ => {
                    return summary_response(
                        state,
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
            let status = match manager.status() {
                Ok(status) => status,
                Err(_) => return unavailable(409),
            };
            if status.relation != "applied" {
                return summary_response(
                    state,
                    409,
                    None,
                    Some(
                        json!({"message":"EMP configuration is not applied; there is no EMP runtime to verify"}),
                    ),
                );
            }
        }
        let result = match sync_runtime(
            state,
            reload_target,
            operation == "reload" || confirmed,
            operation == "verify",
            false,
        ) {
            Ok(result) => result,
            Err(_) => return unavailable(409),
        };
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
        return summary_response(state, if successful { 200 } else { 409 }, None, error);
    }
    let result = match operation {
        "enable" => match crate::services::integration::enable::apply(state) {
            Ok(result) => result,
            Err(crate::services::integration::enable::EnableError::Unavailable(status)) => {
                return unavailable(status);
            }
            Err(crate::services::integration::enable::EnableError::EmptyCatalog) => {
                return summary_response(
                    state,
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
            match restore_native_with_history(manager, Some(&state.backend.integration.search)) {
                Ok((result, _)) => result,
                Err(reason) => {
                    return summary_response(
                        state,
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
        _ => return json_error_response(404, status_text(404), "not found", None, &[]),
    };
    let mut stop_after_restore_response = false;
    if result.ok() {
        if result.state == "active" && crate::services::integration::sync_search(state).is_err() {
            let _ = manager.restore();
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
        let runtime_sync_failed = sync_runtime(
            state,
            Some(if active { "emp" } else { "native" }),
            confirmed,
            false,
            false,
        )
        .is_err();
        // A native restore is already committed here. Runtime observation is
        // secondary; return the restore result before stopping EMP, and leave
        // the service available if the final summary cannot be prepared.
        if runtime_sync_failed && !stop_after_restore_response {
            return unavailable(409);
        }
    }
    let response = summary_response(
        state,
        if result.ok() { 200 } else { 409 },
        Some(&result),
        None,
    );
    if stop_after_restore_response && response.starts_with(b"HTTP/1.1 200 ") {
        if let Some(gate) = restore_admission.take() {
            gate.keep_closed();
        }
        *stop_after_write = true;
    }
    response
}

fn unavailable(status: u16) -> Vec<u8> {
    json_error_response(
        status,
        status_text(status),
        "integration state is unavailable",
        (status == 503).then_some("integration_unavailable"),
        &[],
    )
}

fn summary_response(
    state: &ServerState,
    status: u16,
    result: Option<&IntegrationResult>,
    error: Option<Value>,
) -> Vec<u8> {
    let mut summary = match integration_summary_with_result(state, result) {
        Ok(value) => value,
        Err(_) => return unavailable(503),
    };
    if let Some(error) = error {
        summary["error"] = error;
    }
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &serde_json::to_vec(&summary).expect("integration summary"),
        &[],
    )
}
