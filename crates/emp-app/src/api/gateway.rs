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
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
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
    let by_device = query_values(request.target, "group")
        .first()
        .map(String::as_str)
        == Some("device");
    let devices = by_device.then(|| state.backend.gateway_clients.devices());
    let device_ids = devices.as_ref().map(|devices| {
        devices
            .iter()
            .map(|device| device.id.as_str())
            .collect::<Vec<_>>()
    });
    match state
        .backend
        .usage
        .ledger
        .query_callers(start, end, device_ids.as_deref())
    {
        Ok(mut report) => {
            if let Some(devices) = devices {
                let rows = report["rows"].as_array_mut().expect("caller usage rows");
                let mut recorded: BTreeMap<_, _> = std::mem::take(rows)
                    .into_iter()
                    .map(|row| {
                        (
                            row["caller_id"].as_str().expect("caller ID").to_owned(),
                            row,
                        )
                    })
                    .collect();
                for device in devices {
                    let mut row = recorded.remove(&device.id).unwrap_or_else(|| {
                        json!({
                            "caller_id": device.id,
                            "calls": 0,
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "cost_nanos": 0,
                            "reported_calls": 0,
                            "priced_calls": 0,
                            "models": []
                        })
                    });
                    row["caller_name"] = json!(device.name);
                    rows.push(row);
                }
            }
            json_response(&report)
        }
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
    let backend = &state.backend;
    let result = if request.method == RequestMethod::Delete {
        let Some(id) = body["id"].as_str() else {
            return json_error_response(
                400,
                status_text(400),
                "A device key ID is required",
                None,
                &[],
            );
        };
        backend
            .gateway_clients
            .remove(id, &backend.configuration.vault, &backend.modules)
            .map(|()| json_response(&json!({"ok":true})))
    } else {
        let id = match body.get("id") {
            None => None,
            Some(Value::String(id)) => Some(id.as_str()),
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
        backend
            .gateway_clients
            .issue(
                body["name"].as_str().unwrap_or_default(),
                id,
                &backend.configuration.vault,
                &backend.modules,
            )
            .map(|client| json_response(&client))
    };
    match result {
        Ok(response) => response,
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

fn json_response(value: &impl Serialize) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(value).expect("gateway response"),
        &[("Cache-Control", "no-store")],
    )
}
