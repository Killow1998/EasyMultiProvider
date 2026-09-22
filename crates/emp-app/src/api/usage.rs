//! Existing usage charts and on-demand history refresh endpoints.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, query_values, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use crate::services::accounts::{account_catalog_headers, native_auth_document};
use crate::util::system_now;
use emp_state::usage::{account_owner, ledger::UsageLedger};
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
    let Ok(mut payload) = state.backend.usage.ledger.query(start, end, &category, now) else {
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
        .config
        .lock()
        .ok()
        .map(|config| config.clone())
        .unwrap_or(json!({}));
    let mut names = BTreeMap::new();
    for provider in config["providers"].as_array().into_iter().flatten() {
        let id = provider["id"].as_str().unwrap_or("");
        names.insert(
            ("external", id.to_owned()),
            provider["name"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_owned(),
        );
    }
    for account in config["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
    {
        if let Some(headers) = account_catalog_headers(account, &state.backend.configuration.vault)
        {
            let owner = account_owner(&headers);
            if !owner.is_empty() {
                let name = account["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .or_else(|| account["id"].as_str())
                    .unwrap_or("");
                names
                    .entry(("subscription", owner))
                    .or_insert_with(|| name.to_owned());
            }
        }
    }
    if let Some(headers) = native_auth_document(&state.backend.accounts.native_auth_path)
        .and_then(|auth| emp_codex::account_auth_headers(&auth))
    {
        let owner = account_owner(&headers);
        if !owner.is_empty() {
            names.insert(("native", owner), "Native".into());
        }
    }
    if let Some(groups) = payload["groups"].as_array_mut() {
        for row in groups {
            let category = row["category"].as_str().unwrap_or("").to_owned();
            let owner = row["owner"].as_str().unwrap_or("").to_owned();
            row["owner_name"] = json!(
                names
                    .get(&(category.as_str(), owner.clone()))
                    .map(String::as_str)
                    .unwrap_or("")
            );
            if matches!(category.as_str(), "native" | "subscription")
                && !owner.starts_with("history:")
            {
                row["account_identity_confirmed"] = json!(owner.starts_with("account:"));
            }
        }
    }
    payload["history"] = state.backend.usage.history.status();
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
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
    }
    if let Err(error) = read_json_body(stream, request, prefix, state) {
        return body_error_response(error);
    }
    state.backend.usage.queue_scan();
    json_response(202, &state.backend.usage.history.status())
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
