mod labels;
pub(crate) mod lifetime;
mod sessions;
use crate::app::ServerState;
use crate::http::request::{Request, query_values};
use crate::server::http::error::JsonReply;
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

pub(crate) fn read(
    request: Request<'_>,
    state: &ServerState,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> JsonReply {
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
                return JsonReply::error(404, "Account usage is unavailable", None);
            }
        },
        None => explicit_owner,
    };
    let result = if value("lifetime").as_deref() == Some("true") {
        let Some(owner) = owner else {
            return invalid();
        };
        state.backend.usage.ledger.query_lifetime_with_progress(
            &owner,
            now,
            |_, _| Ok(()),
            cancelled,
        )
    } else if value("series").as_deref() == Some("true")
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
            return sessions::read(request, state, &filter, now, cancelled);
        }
        state.backend.usage.query_series(
            &state.backend.codex.home,
            &filter,
            value("conversation").as_deref() == Some("true"),
            now,
            cancelled,
        )
    } else {
        match owner {
            Some(owner) => state
                .backend
                .usage
                .ledger
                .query_owner(start, end, &owner, now, cancelled),
            None => state
                .backend
                .usage
                .ledger
                .query(start, end, &category, now, cancelled),
        }
    };
    let Ok(mut payload) = result else {
        return JsonReply::error(503, "Usage history is unavailable", None);
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

fn invalid() -> JsonReply {
    JsonReply::error(400, "Invalid usage period or category", None)
}
fn json_response(status: u16, payload: &Value) -> JsonReply {
    JsonReply::new(status, serde_json::to_vec(payload).expect("usage JSON"))
}
