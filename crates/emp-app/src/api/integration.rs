//! Integration mutations and read-only verification of the shared Codex runtime.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::accounts::native_auth_document;
use crate::services::catalog::{refresh_catalog, server_catalog};
use crate::services::integration::integration_summary_with_result;
use crate::services::runtime::sync_runtime;
use emp_integration::IntegrationResult;
use serde_json::{Value, json};
use std::net::TcpStream;
use std::sync::atomic::Ordering;

pub(crate) fn read_integration_request(state: &ServerState) -> Vec<u8> {
    summary_response(state, 200, None, None)
}

pub(crate) fn management_integration_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
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
    let manager = &state.backend.integration.manager;
    let _operation = match manager.operation_lock() {
        Ok(lock) => lock,
        Err(_) => return unavailable(409),
    };
    if matches!(operation, "reload" | "verify") {
        let result = match sync_runtime(
            state,
            None,
            confirmed,
            operation == "verify",
            operation == "reload",
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
        "enable" => {
            let config = match state.backend.configuration.config.lock() {
                Ok(config) => config.clone(),
                Err(_) => return unavailable(503),
            };
            let catalog = server_catalog(state, &config);
            let visible = catalog["models"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|model| {
                    model
                        .get("slug")
                        .and_then(Value::as_str)
                        .is_some_and(|id| !id.is_empty())
                        && model
                            .get("visibility")
                            .and_then(Value::as_str)
                            .unwrap_or("list")
                            == "list"
                });
            if !visible {
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
            let (catalog_path, _) = match refresh_catalog(state) {
                Ok(value) => value,
                Err(_) => return unavailable(503),
            };
            let dynamic = native_auth_document(&state.backend.accounts.native_auth_path)
                .and_then(|auth| emp_state::validate_auth_json(&auth).ok())
                .is_some();
            let base_url = state.base_url.clone();
            let path = catalog_path.to_string_lossy();
            if dynamic {
                let status = match manager.status() {
                    Ok(status) => status,
                    Err(_) => return unavailable(409),
                };
                if status.relation == "applied"
                    && let Some(lease) = &status.lease
                    && lease.fields["openai_base_url"].applied.value.as_deref() == Some(&base_url)
                    && (lease.fields["model_catalog_json"].applied.value.as_deref()
                        == Some(path.as_ref())
                        || (!lease.fields["model_catalog_json"].applied.present
                            && lease.fields[emp_integration::REALTIME_SIDEBAND_FIELD]
                                .applied
                                .value
                                .as_deref()
                                != Some(&base_url)))
                {
                    match manager.restore() {
                        Ok(result) if !result.ok() => {
                            return summary_response(state, 409, Some(&result), None);
                        }
                        Err(_) => return unavailable(409),
                        _ => {}
                    }
                }
            }
            manager.enable_with_sideband(
                &base_url,
                if dynamic { None } else { Some(path.as_ref()) },
                true,
                dynamic.then_some(base_url.as_str()),
            )
        }
        "restore" => {
            if state.backend.integration.search.restore().is_err() {
                return unavailable(409);
            }
            manager.restore()
        }
        _ => return json_error_response(404, status_text(404), "not found", None, &[]),
    };
    let result = match result {
        Ok(result) => result,
        Err(_) => return unavailable(409),
    };
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
        if sync_runtime(
            state,
            Some(if active { "emp" } else { "native" }),
            confirmed,
            false,
            false,
        )
        .is_err()
        {
            return unavailable(409);
        }
    }
    summary_response(
        state,
        if result.ok() { 200 } else { 409 },
        Some(&result),
        None,
    )
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
