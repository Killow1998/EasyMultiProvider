use crate::{
    app::ServerState,
    http::request::{Request, percent_decode, query_values},
    server::http::error::{EntryError, JsonReply},
};
use serde_json::Value;

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

pub(crate) fn refresh(state: &ServerState, id: &str, force: bool) -> JsonReply {
    if local_provider(state, id) {
        return reply(crate::services::claude_cli::quota_query::refresh(state));
    }
    match crate::services::cpa_quota::refresh(state, id, force) {
        Some(value) => reply(Ok(value)),
        None => EntryError::NotFound.reply(),
    }
}

pub(crate) fn history(
    request: Request<'_>,
    state: &ServerState,
    raw_id: &str,
    now: f64,
) -> JsonReply {
    let bound = |key| {
        query_values(request.target, key)
            .into_iter()
            .find_map(|value| value.parse::<i64>().ok())
    };
    let end = bound("end").unwrap_or(now as i64);
    let start = bound("start").unwrap_or(end.saturating_sub(86400));
    if end <= start {
        return JsonReply::error(400, "invalid quota history period", None);
    }
    let id = percent_decode(raw_id, false);
    if local_provider(state, &id) {
        return reply(crate::services::claude_cli::quota_query::history(
            state, start, end,
        ));
    }
    match crate::services::cpa_quota::history(state, &id, start, end) {
        Some(result) => reply(result),
        None => EntryError::NotFound.reply(),
    }
}

fn reply(result: Result<Value, &'static str>) -> JsonReply {
    match result {
        Ok(value) => JsonReply::new(200, serde_json::to_vec(&value).expect("quota JSON")),
        Err(code) => JsonReply::error(503, code, Some(code)),
    }
}
