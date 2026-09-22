//! Api migration.

use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use emp_state::ExportGroups;
use emp_state::export_migration_bundle_with_summary;
use emp_state::import_migration_bundle;
use serde_json::Value;
use std::net::TcpStream;

pub(crate) fn management_migration_request(
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
    let password = body
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if request.raw_path() == "/api/migration/export" {
        let group_values = body.get("groups").and_then(Value::as_array);
        let groups = match group_values {
            Some(values) => {
                let Some(values) = values.iter().map(Value::as_str).collect::<Option<Vec<_>>>()
                else {
                    return json_error_response(
                        400,
                        status_text(400),
                        "select at least one valid export category",
                        None,
                        &[],
                    );
                };
                match ExportGroups::from_list(&values) {
                    Ok(groups) => Some(groups),
                    Err(error) => {
                        return json_error_response(
                            400,
                            status_text(400),
                            &error.to_string(),
                            None,
                            &[],
                        );
                    }
                }
            }
            None => None,
        };
        let config = match state.backend.configuration.config.lock() {
            Ok(config) => config.clone(),
            Err(_) => {
                return json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                );
            }
        };
        let (bundle, summary) = match export_migration_bundle_with_summary(
            &config,
            password,
            &state.backend.configuration.vault,
            groups.as_ref(),
            Some(&state.backend.accounts.native_auth_path),
        ) {
            Ok(result) => result,
            Err(error) => {
                return json_error_response(400, status_text(400), &error.to_string(), None, &[]);
            }
        };
        let summary = serde_json::json!({
            "accounts":summary.accounts,
            "providers":summary.providers,
            "models":summary.models,
            "groups":summary.groups,
            "native_login_included":summary.native_login_included,
            "native_login_missing":summary.native_login_missing,
        })
        .to_string();
        return response(
            "HTTP/1.1 200 OK",
            "application/octet-stream",
            &bundle,
            &[
                ("Cache-Control", "no-store"),
                ("Content-Disposition", "attachment; filename=\"EMP.emp\""),
                ("X-EMP-Export-Summary", &summary),
            ],
        );
    }
    let Some(encoded) = body
        .get("bundle")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return json_error_response(
            400,
            status_text(400),
            "migration bundle is required",
            None,
            &[],
        );
    };
    let bundle = match STANDARD.decode(encoded) {
        Ok(bundle) => bundle,
        Err(_) => {
            return json_error_response(
                400,
                status_text(400),
                "migration bundle is not valid base64",
                None,
                &[],
            );
        }
    };
    let current = match state.backend.configuration.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    let (updated, summary) = match import_migration_bundle(
        &current,
        &bundle,
        password,
        &state.backend.configuration.config_path,
        &state.backend.configuration.vault,
    ) {
        Ok(result) => result,
        Err(error) => {
            return json_error_response(400, status_text(400), &error.to_string(), None, &[]);
        }
    };
    match state.backend.configuration.config.lock() {
        Ok(mut config) => *config = updated,
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "status":"ok",
        "accounts":summary.accounts,
        "providers":summary.providers,
        "models":summary.models,
        "renamed_accounts":summary.renamed_accounts,
    }))
    .expect("migration response is serializable");
    response("HTTP/1.1 200 OK", "application/json", &body, &[])
}
