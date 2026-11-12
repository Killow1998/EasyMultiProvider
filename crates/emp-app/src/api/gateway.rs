use crate::{
    app::ServerState,
    http::{
        auth::same_origin,
        request::{Request, RequestMethod, query_values, read_json_body},
        response::{
            body_error_response, json_error_response, response, status_text, unauthorized_response,
        },
    },
};
use emp_state::usage::ledger::UsageLedger;
use serde_json::{Value, json};
use std::net::TcpStream;

pub(crate) fn read(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    if request.raw_path().ends_with("/clients") {
        return json_response(&json!({"clients":state.backend.gateway_clients.list()}));
    }
    let now = crate::util::system_now();
    let number = |name, default| {
        query_values(request.target, name)
            .first()
            .map_or(Ok(default), |value| value.parse::<f64>())
    };
    let (start, end) = match (number("start", now - 86400.0), number("end", now)) {
        (Ok(start), Ok(end)) if UsageLedger::valid_period(start, end, "all") => (start, end),
        _ => {
            return json_error_response(400, status_text(400), "Invalid report period", None, &[]);
        }
    };
    match state.backend.usage.ledger.query_callers(start, end) {
        Ok(report) => json_response(&report),
        Err(_) => json_error_response(
            503,
            status_text(503),
            "Caller usage is unavailable",
            None,
            &[],
        ),
    }
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
    let result = if request.method == RequestMethod::Delete {
        let Some(id) = body["id"].as_str().filter(|id| !id.is_empty()) else {
            return json_error_response(
                400,
                status_text(400),
                "A device key ID is required",
                None,
                &[],
            );
        };
        state
            .backend
            .gateway_clients
            .remove(id, &state.backend.configuration.vault)
            .map(|()| json!({"ok":true}))
    } else {
        let id = match body.get("id") {
            None => None,
            Some(Value::String(id)) if !id.is_empty() => Some(id.as_str()),
            Some(_) => {
                return json_error_response(
                    400,
                    status_text(400),
                    "A device key ID is required",
                    None,
                    &[],
                );
            }
        };
        state.backend.gateway_clients.issue(
            body["name"].as_str().unwrap_or_default(),
            id,
            &state.backend.configuration.vault,
        )
    };
    match result {
        Ok(value) => json_response(&value),
        Err(error) => {
            let (status, message) = if error.kind() == std::io::ErrorKind::InvalidInput {
                (400, error.to_string())
            } else {
                (503, "Gateway clients could not be saved".into())
            };
            json_error_response(status, status_text(status), &message, None, &[])
        }
    }
}

fn json_response(value: &Value) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(value).expect("gateway response"),
        &[("Cache-Control", "no-store")],
    )
}
