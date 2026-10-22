//! Page main-conversation bills using reconciled usage and local ownership metadata.
use super::*;
use emp_state::usage::ledger::SessionBillPage;

pub(super) fn read(
    request: Request<'_>,
    state: &ServerState,
    filter: &UsageSeriesFilter,
    now: f64,
) -> Vec<u8> {
    let value = |name: &str| query_values(request.target, name).first().cloned();
    let number =
        |name: &str, default: usize| value(name).map_or(Ok(default), |raw| raw.parse::<usize>());
    let (Ok(offset), Ok(limit)) = (number("offset", 0), number("limit", 20)) else {
        return invalid();
    };
    let sort = value("sort").unwrap_or_else(|| "recent".into());
    let query = value("q").unwrap_or_default().trim().to_lowercase();
    if !(1..=100).contains(&limit)
        || offset > i64::MAX as usize
        || !["recent", "tokens", "cost"].contains(&sort.as_str())
        || query.len() > 512
        || query.chars().any(char::is_control)
    {
        return invalid();
    }
    let page = SessionBillPage {
        offset,
        limit,
        sort,
        matching_sessions: None,
    };
    let Ok(mut payload) = state.backend.usage.query_sessions(
        &state.backend.accounts.codex_home,
        filter,
        page,
        &query,
        now,
    ) else {
        return unavailable();
    };
    payload["history"] = state.backend.usage.status();
    json_response(200, &payload)
}
fn unavailable() -> Vec<u8> {
    json_error_response(
        503,
        status_text(503),
        "Session usage is unavailable",
        None,
        &[],
    )
}
