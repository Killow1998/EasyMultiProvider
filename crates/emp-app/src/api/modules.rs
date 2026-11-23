use crate::server::http::management::ManagementSession;
use crate::{
    app::ServerState,
    http::{
        request::{Request, read_json_body},
        response::{body_error_response, json_error_response, response, status_text},
    },
    server::http::error::JsonReply,
    services::modules::Action,
};
use emp_modules::Module;
use serde::Deserialize;
use serde_json::json;
use std::net::TcpStream;

pub(crate) fn read(state: &ServerState) -> JsonReply {
    JsonReply::new(
        200,
        serde_json::to_vec(&json!({
            "modules":state.backend.modules.status(),
            "migration_allowed":!state.backend.team.migration_restricted(),
        }))
        .expect("module status"),
    )
    .with_header("Cache-Control", "no-store")
}

pub(crate) fn mutate(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if let Err(error) = ManagementSession::require(request, state, now) {
        return error.wire_response();
    }
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if body["action"] == "check_updates" {
        return match state.backend.modules.check_updates(&state.updates.manager) {
            Ok(updates) => response(
                "HTTP/1.1 200 OK",
                "application/json",
                &serde_json::to_vec(&json!({"updates":updates})).expect("module updates"),
                &[("Cache-Control", "no-store")],
            ),
            Err(error) => json_error_response(
                409,
                status_text(409),
                "Could not check EMP module updates",
                Some(&error),
                &[],
            ),
        };
    }
    let Ok(module) = Module::deserialize(&body["id"]) else {
        return json_error_response(400, status_text(400), "Unknown EMP module", None, &[]);
    };
    let Ok(action) = Action::deserialize(&body["action"]) else {
        return json_error_response(
            409,
            status_text(409),
            "The EMP module could not be changed",
            Some("invalid_module_action"),
            &[],
        );
    };
    let change = || {
        state.backend.modules.change(
            module,
            action,
            body["revision"].as_str(),
            &state.updates.manager,
            || {
                if module == Module::Team {
                    state
                        .backend
                        .team
                        .purge_local(state)
                        .map_err(|error| error.1)?;
                }
                Ok(())
            },
        )
    };
    let result = if module == Module::Team {
        state.backend.team.change_module(state, change)
    } else {
        change()
    };
    match result {
        Ok(()) => {
            if matches!(module, Module::Aggregate | Module::Team | Module::Aero)
                && crate::services::catalog::refresh_catalog(state).is_err()
            {
                crate::services::account_catalog::request_refresh(state, false);
            }
            read(state).wire_response()
        }
        Err(error) => json_error_response(
            409,
            status_text(409),
            "The EMP module could not be changed",
            Some(&error),
            &[],
        ),
    }
}
