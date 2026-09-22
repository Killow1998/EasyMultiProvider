//! Api integration.

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
use crate::services::catalog::refresh_catalog;
use crate::services::integration::integration_summary;
use serde_json::Value;
use std::net::TcpStream;
use std::sync::atomic::Ordering;

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
    if body.get("confirm_reload").and_then(Value::as_bool) != Some(true) {
        let mut summary = match integration_summary(state) {
            Ok(summary) => summary,
            Err(error) => {
                return json_error_response(503, status_text(503), &error, None, &[]);
            }
        };
        summary["error"] = serde_json::json!({
            "message":"Confirmation is required before changing Codex integration files"
        });
        let body = serde_json::to_vec(&summary).unwrap();
        return response("HTTP/1.1 409 Conflict", "application/json", &body, &[]);
    }
    let operation = request.raw_path().rsplit('/').next().unwrap_or_default();
    let result = match operation {
        "enable" => {
            let (catalog, _) = match refresh_catalog(state) {
                Ok(result) => result,
                Err(()) => {
                    return json_error_response(
                        500,
                        status_text(500),
                        "internal server error",
                        None,
                        &[],
                    );
                }
            };
            state.backend.integration.manager.enable(
                &format!("http://127.0.0.1:{}/v1", state.port),
                Some(&catalog.to_string_lossy()),
                true,
            )
        }
        "restore" => state.backend.integration.manager.restore(),
        "reload" | "verify" => {
            return match integration_summary(state) {
                Ok(summary) => {
                    let body = serde_json::to_vec(&summary).unwrap();
                    response("HTTP/1.1 200 OK", "application/json", &body, &[])
                }
                Err(error) => json_error_response(503, status_text(503), &error, None, &[]),
            };
        }
        _ => {
            return json_error_response(404, status_text(404), "not found", None, &[]);
        }
    };
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            return json_error_response(409, status_text(409), &error.to_string(), None, &[]);
        }
    };
    if result.ok() && result.state == "active" {
        state
            .backend
            .integration
            .owned
            .store(true, Ordering::Release);
    } else if result.ok() && result.state == "restored" {
        state
            .backend
            .integration
            .owned
            .store(false, Ordering::Release);
    }
    match integration_summary(state) {
        Ok(summary) => {
            let body = serde_json::to_vec(&summary).unwrap();
            response("HTTP/1.1 200 OK", "application/json", &body, &[])
        }
        Err(error) => json_error_response(503, status_text(503), &error, None, &[]),
    }
}
