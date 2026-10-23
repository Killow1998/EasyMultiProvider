//! Quota sampling deadlines and the shutdown wake share one wait lock.

use super::QuotaSampleCounts;
use crate::app::ServerState;
use crate::services::accounts::{duplicate_accounts, regular_file};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[cfg(not(test))]
const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(44);
/// Tests drive quota checks explicitly rather than invoking a background helper.
#[cfg(test)]
const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub(crate) fn run(state: &Arc<ServerState>) {
    super::migrate_legacy_quota_history(state);
    // Sample right away so quota is ready when the page first opens.
    let mut deadline = if cfg!(test) {
        Instant::now() + QUOTA_SAMPLE_INTERVAL
    } else {
        Instant::now()
    };
    while !state.shutdown.load(Ordering::Acquire) {
        let mut wait = match state.backend.accounts.quota_sampler_wait.lock() {
            Ok(wait) => wait,
            Err(_) => return,
        };
        loop {
            if state.shutdown.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let result = state
                .backend
                .accounts
                .quota_sampler_condition
                .wait_timeout(wait, deadline.saturating_duration_since(now));
            match result {
                Ok((next, _)) => wait = next,
                Err(_) => return,
            }
        }
        drop(wait);
        deadline = Instant::now() + QUOTA_SAMPLE_INTERVAL;
        if !state.shutdown.load(Ordering::Acquire) {
            super::sample_quotas_once(state);
            // Disabled or duplicate accounts are not sampled;
            // their rotated credentials still need saving.
            super::flush_pending_rotations(state);
        }
    }
}

fn quota_sample_targets(state: &ServerState) -> Vec<String> {
    let mut targets = Vec::new();
    if regular_file(&state.backend.accounts.native_auth_path)
        && !matches!(
            emp_integration::local_auth::mode(&state.backend.accounts.native_auth_path),
            "local_key" | "api_key"
        )
    {
        targets.push("@native".to_owned());
    }
    let Some(config) = state
        .backend
        .configuration
        .read()
        .ok()
        .map(|config| config.clone())
    else {
        return targets;
    };
    // Sample each upstream account once: an import of the native login or of
    // another imported account shares its quota and history owner.
    let duplicates = duplicate_accounts(
        &config,
        &state.backend.configuration.vault,
        &state.backend.accounts.native_auth_path,
    );
    let accounts = config
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for account in accounts {
        let Some(account_id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        if account
            .get("auth_file")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            continue;
        }
        if !duplicates.contains_key(account_id) {
            targets.push(account_id.to_owned());
        }
    }
    targets
}

/// Each scoped worker owns its count; even a panicked worker keeps prior counts.
pub(crate) fn sample_quotas_once(state: &ServerState) -> QuotaSampleCounts {
    let targets = quota_sample_targets(state);
    let worker_count = 4.min(targets.len());
    let mut counts = vec![QuotaSampleCounts::default(); worker_count];
    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for (offset, count) in counts.iter_mut().enumerate() {
            let targets = &targets;
            if let Ok(worker) = std::thread::Builder::new()
                .name("emp-quota-refresh".to_owned())
                .spawn_scoped(scope, move || {
                    for account_id in targets.iter().skip(offset).step_by(worker_count) {
                        if state.shutdown.load(Ordering::Acquire) {
                            return;
                        }
                        if super::refresh_account_serialized(state, account_id).is_ok() {
                            count.sampled += 1;
                        } else {
                            count.failed += 1;
                        }
                    }
                })
            {
                workers.push(worker);
            }
        }
        for worker in workers {
            let _ = worker.join();
        }
    });
    counts
        .into_iter()
        .fold(QuotaSampleCounts::default(), |mut total, count| {
            total.sampled += count.sampled;
            total.failed += count.failed;
            total
        })
}
