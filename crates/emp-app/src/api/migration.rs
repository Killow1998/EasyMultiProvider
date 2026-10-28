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
use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    let Some(session) = request
        .session_token()
        .filter(|token| state.sessions.contains(Some(token.as_str()), now))
    else {
        return unauthorized_response();
    };
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let operation = request.raw_path().rsplit('/').next().unwrap_or_default();
    let lan = request.raw_path().starts_with("/api/migration/lan/");
    let result = if lan {
        if operation == "start"
            && !state.sessions.consume_export_confirmation(
                body["confirmation"].as_str().unwrap_or_default(),
                EXPORT_CONFIRMATION_OPERATION,
                now,
            )
        {
            return json_error_response(
                403,
                status_text(403),
                "export confirmation is required",
                Some("export_confirmation_required"),
                &[],
            );
        }
        crate::services::migration::lan::execute(state, &session, operation, &body)
    } else if matches!(operation, "decode" | "preview" | "demo")
        || (operation == "import" && body.get("token").is_some())
    {
        crate::services::migration::preview_request(
            request.observation_id,
            state,
            &session,
            operation,
            &body,
        )
    } else if request.raw_path() == "/api/migration/export/confirm" {
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
    } else if request.raw_path() == "/api/migration/export" {
        // Credential exports consume a confirmation issued by a separate request.
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
            Ok(exported) if body.get("selections").is_some() => response(
                "HTTP/1.1 200 OK",
                "application/json",
                &serde_json::to_vec(&serde_json::json!({
                    "bundle": STANDARD.encode(exported.bundle),
                    "summary": exported.summary,
                }))
                .expect("export is JSON"),
                &[],
            ),
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
    } else {
        crate::services::migration::import(request.observation_id, state, &body)
    };
    match result {
        Ok(body) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&body).expect("migration response is serializable"),
            if lan {
                &[("Cache-Control", "no-store")]
            } else {
                &[]
            },
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
