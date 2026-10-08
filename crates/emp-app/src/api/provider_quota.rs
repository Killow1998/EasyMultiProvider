//! Provider-owned quota reads behind the management session boundary.
use crate::{
    app::ServerState,
    http::{
        auth::same_origin,
        request::{Request, percent_decode, query_values, read_json_body},
        response::*,
    },
};
use serde_json::Value;
use std::net::TcpStream;

fn local_provider(state: &ServerState, id: &str) -> bool {
    state
        .backend
        .configuration
        .read()
        .ok()
        .is_some_and(|config| {
            config["providers"].as_array().is_some_and(|providers| {
                providers.iter().any(|p| {
                    p["id"] == id
                        && p["execution_backend"] == "claude_cli"
                        && p["auth_mode"] == "claude_login"
                })
            })
        })
}

pub(crate) fn refresh(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
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
    if let Err(error) = read_json_body(stream, request, prefix, state) {
        return body_error_response(error);
    }
    let id = request
        .raw_path()
        .strip_prefix("/api/providers/")
        .and_then(|path| path.strip_suffix("/quota"))
        .map(|id| percent_decode(id, false))
        .unwrap_or_default();
    if !local_provider(state, &id) {
        return not_found_response();
    }
    reply(crate::services::claude_cli::quota_query::refresh(state))
}

/// Called only after the common GET management-session check.
pub(crate) fn history(request: Request<'_>, state: &ServerState, id: &str, now: f64) -> Vec<u8> {
    if !local_provider(state, id) {
        return not_found_response();
    }
    let bound = |key| {
        query_values(request.target, key)
            .into_iter()
            .find_map(|value| value.parse::<i64>().ok())
    };
    let end = bound("end").unwrap_or(now as i64);
    let start = bound("start").unwrap_or(end.saturating_sub(86400));
    if end <= start {
        return json_error_response(
            400,
            status_text(400),
            "invalid quota history period",
            None,
            &[],
        );
    }
    reply(crate::services::claude_cli::quota_query::history(
        state, start, end,
    ))
}

fn reply(result: Result<Value, &'static str>) -> Vec<u8> {
    match result {
        Ok(value) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&value).expect("quota JSON"),
            &[],
        ),
        Err(code) => json_error_response(503, status_text(503), code, Some(code), &[]),
    }
}
