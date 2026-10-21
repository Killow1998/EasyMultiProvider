//! Page session bills from reconciled usage and resolve only local index titles.
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
    let ledger = &state.backend.usage.ledger;
    let directory = &state.backend.usage.sessions;
    let home = &state.backend.accounts.codex_home;
    let matching_sessions = if query.is_empty() {
        None
    } else {
        let Ok(ids) = ledger.session_ids(filter) else {
            return unavailable();
        };
        let names = directory.resolve(home, &ids);
        Some(
            ids.into_iter()
                .filter(|id| {
                    id.to_lowercase().contains(&query)
                        || names
                            .get(id)
                            .is_some_and(|name| name.to_lowercase().contains(&query))
                })
                .collect(),
        )
    };
    let page = SessionBillPage {
        offset,
        limit,
        sort,
        matching_sessions,
    };
    let Ok(mut payload) = ledger.query_sessions(filter, &page, now) else {
        return unavailable();
    };
    let ids = payload["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["session_id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let names = directory.resolve(home, &ids);
    for row in payload["records"].as_array_mut().into_iter().flatten() {
        if let Some(name) = row["session_id"].as_str().and_then(|id| names.get(id)) {
            row["session_name"] = json!(name);
        }
    }
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
