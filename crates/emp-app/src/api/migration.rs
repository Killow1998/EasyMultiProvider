use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::server::http::management::{
    ManagementSession, consume_export_confirmation, issue_export_confirmation,
};
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
    let session = match ManagementSession::require(request, state, now) {
        Ok(ManagementSession(session)) => session,
        Err(error) => return error.wire_response(),
    };
    if let Err(crate::services::team::Failure(status, code)) =
        state.backend.team.check_migration(state)
    {
        return json_error_response(
            status,
            status_text(status),
            if code == "workspace_migration_permission_required" {
                "Service migration requires a team administrator"
            } else {
                "Could not confirm team permissions for service migration"
            },
            Some(&code),
            &[],
        );
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let operation = request.raw_path().rsplit('/').next().unwrap_or_default();
    let lan = request.raw_path().starts_with("/api/migration/lan/");
    let result = if lan {
        if operation == "start"
            && let Err(error) = consume_export_confirmation(
                state,
                body["confirmation"].as_str().unwrap_or_default(),
                request.raw_path(),
                now,
            )
        {
            return error.wire_response();
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
        return match issue_export_confirmation(state, now) {
            Ok(reply) => reply.wire_response(),
            Err(error) => error.wire_response(),
        };
    } else if request.raw_path() == "/api/migration/export" {
        let confirmation = body
            .get("confirmation")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Err(error) =
            consume_export_confirmation(state, confirmation, request.raw_path(), now)
        {
            return error.wire_response();
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
