//! Existing usage charts and on-demand history refresh endpoints.
mod labels;
mod sessions;
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, query_values, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::accounts::{
    account_catalog_headers, native_subscription_headers, quota_owner_key,
};
use crate::util::system_now;
use emp_state::usage::{
    account_owner,
    ledger::{UsageLedger, UsageSeriesFilter},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpStream;

pub(crate) fn read(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let now = system_now();
    let parse = |name: &str, default: f64| {
        query_values(request.target, name)
            .first()
            .map_or(Ok(default), |value| value.parse::<f64>())
    };
    let (Ok(start), Ok(end)) = (parse("start", now - 86400.0), parse("end", now)) else {
        return invalid();
    };
    let category = query_values(request.target, "category")
        .first()
        .cloned()
        .unwrap_or_else(|| "all".into());
    if !UsageLedger::valid_period(start, end, &category) {
        return invalid();
    }
    let value = |name: &str| {
        query_values(request.target, name)
            .into_iter()
            .find(|v| !v.is_empty())
    };
    let account_id = value("account_id").or_else(|| value("account"));
    let explicit_owner = value("owner");
    if (account_id.is_some() && explicit_owner.is_some())
        || explicit_owner
            .as_ref()
            .is_some_and(|owner| owner.len() > 256 || owner.chars().any(char::is_control))
    {
        return invalid();
    }
    let owner = match account_id {
        Some(id) => match quota_owner_key(state, &id) {
            Ok(owner) => Some(owner),
            Err(_) => {
                return json_error_response(
                    404,
                    status_text(404),
                    "Account usage is unavailable",
                    None,
                    &[],
                );
            }
        },
        None => explicit_owner,
    };
    let result = if value("series").as_deref() == Some("true")
        || value("sessions").as_deref() == Some("true")
    {
        let filter = UsageSeriesFilter {
            start,
            end,
            category,
            owner,
            provider: value("provider"),
            model: value("model"),
            session: value("session"),
            state: value("state"),
        };
        if [
            &filter.provider,
            &filter.model,
            &filter.session,
            &filter.state,
        ]
        .into_iter()
        .flatten()
        .any(|v| v.len() > 256 || v.chars().any(char::is_control))
        {
            return invalid();
        }
        if value("sessions").as_deref() == Some("true") {
            return sessions::read(request, state, &filter, now);
        }
        state.backend.usage.query_series(
            &state.backend.accounts.codex_home,
            &filter,
            value("conversation").as_deref() == Some("true"),
            now,
        )
    } else {
        match owner {
            Some(owner) => state
                .backend
                .usage
                .ledger
                .query_owner(start, end, &owner, now),
            None => state.backend.usage.ledger.query(start, end, &category, now),
        }
    };
    let Ok(mut payload) = result else {
        return json_error_response(
            503,
            status_text(503),
            "Usage history is unavailable",
            None,
            &[],
        );
    };
    let config = state
        .backend
        .configuration
        .read()
        .ok()
        .map(|config| config.clone())
        .unwrap_or(json!({}));
    labels::enrich(state, &config, &mut payload);
    payload["history"] = state.backend.usage.status();
    json_response(200, &payload)
}
pub(crate) fn scan(
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
    let Some(id) = state.backend.usage.queue_scan(state) else {
        return json_error_response(503, status_text(503), "EMP is shutting down", None, &[]);
    };
    let mut status = state.backend.usage.status();
    status["command_id"] = json!(id);
    json_response(202, &status)
}
fn invalid() -> Vec<u8> {
    json_error_response(
        400,
        status_text(400),
        "Invalid usage period or category",
        None,
        &[],
    )
}
fn json_response(status: u16, payload: &Value) -> Vec<u8> {
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &serde_json::to_vec(payload).expect("usage JSON"),
        &[],
    )
}
