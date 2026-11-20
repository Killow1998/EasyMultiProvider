use crate::{
    app::ServerState,
    http::{
        auth::same_origin,
        request::{Request, read_json_body},
        response::{
            body_error_response, json_error_response, response, status_text, unauthorized_response,
        },
    },
};
use emp_modules::Module;
use serde_json::json;
use std::net::TcpStream;

pub(crate) fn unavailable(path: &str, state: &ServerState) -> Option<Vec<u8>> {
    let module = match path {
        "/v1/decisions"
        | "/v1/systemone"
        | "/api/decisions/models"
        | "/api/decisions/connection" => Module::Decision,
        "/api/gateway" | "/api/gateway/clients" | "/api/gateway/usage" => Module::Gateway,
        _ if path.starts_with("/api/team/") => Module::Team,
        _ => return None,
    };
    (!state.backend.modules.active(module)).then(|| {
        json_error_response(
            404,
            status_text(404),
            "This EMP module is not active",
            Some("module_disabled"),
            &[],
        )
    })
}

pub(crate) fn read(state: &ServerState) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(&json!({
            "modules":state.backend.modules.status(),
            "migration_allowed":!state.backend.team.migration_restricted(),
        }))
        .expect("module status"),
        &[("Cache-Control", "no-store")],
    )
}

pub(crate) fn mutate(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port)
        || !state
            .sessions
            .contains(request.session_token().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let module = serde_json::from_value::<Module>(body["id"].clone());
    let action = body["action"].as_str().unwrap_or_default();
    let Ok(module) = module else {
        return json_error_response(400, status_text(400), "Unknown EMP module", None, &[]);
    };
    match state
        .backend
        .modules
        .change(module, action, &state.updates.manager, || {
            if module == Module::Team {
                state
                    .backend
                    .team
                    .remove_module_data(state)
                    .map_err(|error| error.1)?;
            }
            Ok(())
        }) {
        Ok(()) => read(state),
        Err(error) => json_error_response(
            409,
            status_text(409),
            "The EMP module could not be changed",
            Some(&error),
            &[],
        ),
    }
}
