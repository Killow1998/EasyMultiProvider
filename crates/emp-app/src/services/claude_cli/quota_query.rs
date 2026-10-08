//! Bounded Claude CLI control queries. Authentication and network access stay
//! inside the installed CLI; EMP sends no user messages and reads no OAuth tokens.
use super::{local_query::with_login, process};
use crate::app::ServerState;
use serde_json::{Value, json};

pub(crate) fn refresh(state: &ServerState) -> Result<Value, &'static str> {
    // All local-login providers share one OS login. Concurrent clicks reuse the
    // result completed while they waited, rather than querying once per alias.
    let requested_at = crate::util::system_now();
    let _guard = state
        .backend
        .claude_quota
        .refresh_lock
        .lock()
        .map_err(|_| "claude_quota_unavailable")?;
    if state.backend.claude_quota.last_refresh() >= requested_at {
        return Ok(json!({"quota":state.backend.claude_quota.snapshot(requested_at)}));
    }
    let result = query(state);
    state.backend.diagnostics.journal.event(
        if result.is_ok() { "info" } else { "warning" },
        "claude_quota_refresh",
        &json!({"success":result.is_ok(),"error_code":result.as_ref().err()}),
    );
    result
}

fn query(state: &ServerState) -> Result<Value, &'static str> {
    with_login(state, true, |config, generation, cancelled| {
        let stdout = process::run_quota(process::control_command(config), cancelled, || {
            state
                .shutdown
                .load(std::sync::atomic::Ordering::Acquire)
                .then_some(process::CancellationReason::ServerShutdown)
        })?;
        let reply = stdout
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .find(|event| {
                event["type"] == "control_response"
                    && event["response"]["request_id"] == "emp-quota"
            })
            .ok_or("claude_quota_unsupported")?;
        if reply["response"]["subtype"] != "success" {
            return Err("claude_quota_unsupported");
        }
        let usage = &reply["response"]["response"];
        if usage["rate_limits_available"] != true {
            return Err("claude_quota_not_available");
        }
        let now = crate::util::system_now();
        if !state
            .backend
            .claude_quota
            .record_usage(generation, usage, now)
        {
            return Err("claude_quota_not_available");
        }
        let saved = publish(state);
        Ok(json!({"quota":state.backend.claude_quota.snapshot(now),"history_saved":saved}))
    })
}

pub(crate) fn history(state: &ServerState, start: i64, end: i64) -> Result<Value, &'static str> {
    // Resolve the current login even after EMP restarted; keep old identities in
    // separate history keys when Claude logs out or changes account.
    with_login(state, false, |_, _, _| {
        state.backend.claude_quota.history(start, end)
    })
}

pub(super) fn publish(state: &ServerState) -> bool {
    let saved = state.backend.claude_quota.persist().is_ok();
    state.backend.diagnostics.journal.event(
        if saved { "info" } else { "warning" },
        "claude_quota_observed",
        &json!({"history_saved":saved}),
    );
    state
        .backend
        .management_events
        .publish(crate::services::management_events::Change::Quota);
    saved
}
