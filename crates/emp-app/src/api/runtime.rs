//! Management of installed Codex runtimes, separate from shared runtime state.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::runtime::{compatibility_snapshot, runtime_preferences};
use serde_json::{Value, json};
use std::net::TcpStream;

pub(crate) fn management_request(
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
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let value = if request.raw_path() == "/api/runtime/scan" {
        Ok(compatibility_snapshot(state, true))
    } else {
        select(state, body.get("sources"))
    };
    match value {
        Ok(value) => response(
            "HTTP/1.1 200 OK",
            "application/json",
            &serde_json::to_vec(&value).expect("runtime inventory JSON"),
            &[],
        ),
        Err((status, message)) => {
            json_error_response(status, status_text(status), message, None, &[])
        }
    }
}
fn select(state: &ServerState, sources: Option<&Value>) -> Result<Value, (u16, &'static str)> {
    let source = sources
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or((400, "at least one runtime source is required"))?;
    let mut selected = Vec::new();
    for source in source {
        let source = source
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or((400, "runtime source is invalid"))?;
        if !selected.contains(&source) {
            selected.push(source);
        }
    }
    if selected.contains(&"auto") && selected.len() != 1 {
        return Err((400, "automatic runtime selection cannot be combined"));
    }
    let current = compatibility_snapshot(state, true);
    if selected != ["auto"]
        && selected.iter().any(|source| {
            !current["runtimes"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|runtime| runtime["source"] == *source && runtime["selectable"] == true)
        })
    {
        return Err((400, "Codex runtime is unavailable or incompatible"));
    }
    let configuration = &state.backend.configuration;
    let mut current = configuration
        .config
        .lock()
        .map_err(|_| (500, "internal server error"))?;
    let mut updated = current.clone();
    updated["codex_runtime_sources"] = json!(selected);
    let saved =
        emp_state::with_file_transaction(|transaction| -> Result<Value, emp_state::ConfigError> {
            emp_state::save_configuration_in_transaction(
                &updated,
                Some(&configuration.config_path),
                &configuration.vault,
                transaction,
            )?;
            emp_state::load_configuration(Some(&configuration.config_path))
        })
        .map_err(|_| (500, "internal server error"))?;
    *current = saved;
    drop(current);
    Ok(state
        .backend
        .integration
        .inventory
        .snapshot(&runtime_preferences(state), true))
}
