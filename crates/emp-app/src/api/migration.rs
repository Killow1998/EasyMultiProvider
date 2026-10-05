//! Api migration.

use crate::app::ServerState;
use crate::http::auth::EXPORT_CONFIRMATION_LIFETIME_SECONDS;
use crate::http::auth::EXPORT_CONFIRMATION_OPERATION;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
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
        .contains(request.session_token().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if request.raw_path() == "/api/migration/export/confirm" {
        let Some(confirmation) = state
            .sessions
            .issue_export_confirmation(EXPORT_CONFIRMATION_OPERATION, now)
        else {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "confirmation": confirmation,
            "expires_in": EXPORT_CONFIRMATION_LIFETIME_SECONDS as u64,
        }))
        .expect("export confirmation is JSON serializable");
        return response("HTTP/1.1 200 OK", "application/json", &body, &[]);
    }
    if request.raw_path() == "/api/migration/export" {
        // Exports carry every credential, so each one needs a fresh
        // confirmation issued by a separate request just before it.
        let confirmation = body
            .get("confirmation")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !state.sessions.consume_export_confirmation(
            confirmation,
            EXPORT_CONFIRMATION_OPERATION,
            now,
        ) {
            return json_error_response(
                403,
                status_text(403),
                "export confirmation is missing or expired; confirm the export again",
                Some("export_confirmation_required"),
                &[],
            );
        }
        return match crate::services::migration::export(request.observation_id, state, &body) {
            Ok(exported) => response(
                "HTTP/1.1 200 OK",
                "application/octet-stream",
                &exported.bundle,
                &[
                    ("Cache-Control", "no-store"),
                    ("Content-Disposition", "attachment; filename=\"EMP.emp\""),
                    ("X-EMP-Export-Summary", &exported.summary.to_string()),
                ],
            ),
            Err(error) => migration_error(error),
        };
    }
    match crate::services::migration::import(request.observation_id, state, &body) {
        Ok(body) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&body).expect("migration response is serializable"),
            &[],
        ),
        Err(error) => migration_error(error),
    }
}

fn migration_error(error: crate::services::migration::MigrationError) -> Vec<u8> {
    match error {
        crate::services::migration::MigrationError::Invalid { message, code } => {
            json_error_response(400, status_text(400), &message, code, &[])
        }
        crate::services::migration::MigrationError::Internal => {
            json_error_response(500, status_text(500), "internal server error", None, &[])
        }
    }
}
