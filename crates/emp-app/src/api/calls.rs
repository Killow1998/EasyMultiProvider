//! One authenticated report interface for call details and measured performance.
use crate::app::ServerState;
use crate::http::request::{Request, query_values};
use crate::http::response::{json_error_response, response, status_text};
use emp_state::usage::ledger::CallFilter;
use serde_json::json;

pub(crate) fn read(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let now = crate::util::system_now();
    let value = |name: &str| query_values(request.target, name).first().cloned();
    let number =
        |name: &str, default: f64| value(name).map_or(Ok(default), |raw| raw.parse::<f64>());
    let (Ok(start), Ok(end)) = (number("start", now - 86400.0), number("end", now)) else {
        return invalid();
    };
    let integer =
        |name: &str, default: usize| value(name).map_or(Ok(default), |raw| raw.parse::<usize>());
    let (Ok(offset), Ok(limit), Ok(models_offset)) = (
        integer("offset", 0),
        integer("limit", 50),
        integer("models_offset", 0),
    ) else {
        return invalid();
    };
    let filter = CallFilter {
        start,
        end,
        offset,
        limit,
        models_offset,
        models_sort: value("models_sort").unwrap_or_else(|| "calls".into()),
        category: value("category"),
        provider: value("provider"),
        account: value("account"),
        model: value("model"),
        models: query_values(request.target, "models"),
        session: value("session"),
        state: value("state"),
        request: value("request"),
    };
    if !["calls", "model", "duration", "ttft", "tps"].contains(&filter.models_sort.as_str())
        || filter
            .category
            .as_deref()
            .is_some_and(|category| !emp_state::usage::CATEGORIES.contains(&category))
        || filter.models.len() > 64
        || !emp_state::usage::ledger::UsageLedger::valid_period(start, end, "all")
        || !(1..=100).contains(&limit)
    {
        return invalid();
    }
    let Ok(mut report) = state.backend.usage.ledger.query_calls(&filter) else {
        return json_error_response(
            503,
            status_text(503),
            "Call history is unavailable",
            None,
            &[],
        );
    };
    let ids = report["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            row["session_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    let names = state
        .backend
        .usage
        .sessions
        .resolve(&state.backend.accounts.codex_home, &ids);
    for row in report["records"].as_array_mut().into_iter().flatten() {
        if let Some(name) = row["session_id"].as_str().and_then(|id| names.get(id)) {
            row["session_name"] = json!(name);
        }
    }
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(&report).expect("call report"),
        &[],
    )
}
fn invalid() -> Vec<u8> {
    json_error_response(400, status_text(400), "Invalid call filter", None, &[])
}
