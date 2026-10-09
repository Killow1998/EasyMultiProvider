//! Quota sampling deadlines and the shutdown wake share one wait lock.

use crate::app::ServerState;
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
