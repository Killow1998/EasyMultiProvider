use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) const CATALOG_MAX_AGE: Duration = Duration::from_secs(15 * 60);
pub(crate) const CATALOG_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

const BASE_RETRY: Duration = Duration::from_secs(30);
const MAX_RETRY: Duration = Duration::from_secs(10 * 60);
const AUTH_RETRY: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub(crate) struct CatalogRefreshState {
    queue: Mutex<QueueState>,
    wake: Condvar,
    poll_gate: Mutex<()>,
}

#[derive(Default)]
struct QueueState {
    pending: bool,
    force: bool,
    running: bool,
    catalog_publication_pending: bool,
    account_generations: BTreeMap<String, u64>,
    polls: BTreeMap<String, PollState>,
}

#[derive(Default)]
struct PollState {
    fingerprint: String,
    generation: u64,
    expires_at: Option<Instant>,
    retry_after: Option<Instant>,
    failures: u32,
}

impl CatalogRefreshState {
    pub(crate) fn request(&self, force: bool) {
        if let Ok(mut queue) = self.queue.lock() {
            // Requests arriving during a pass join that pass. Its source
            // snapshot is revalidated before persistence, and the next scan
            // observes any identity change that raced the request.
            if !queue.running || !force {
                queue.pending = true;
                if !queue.running {
                    queue.force |= force;
                }
            }
            self.wake.notify_one();
        }
    }

    pub(crate) fn account_changed(&self, id: &str) {
        if let Ok(mut queue) = self.queue.lock() {
            let generation = queue.account_generations.entry(id.to_owned()).or_default();
            *generation = generation.wrapping_add(1);
            queue.polls.remove(id);
            queue.catalog_publication_pending = true;
            queue.pending = true;
            self.wake.notify_one();
        }
    }

    pub(crate) fn account_generation(&self, id: &str) -> u64 {
        self.queue
            .lock()
            .ok()
            .and_then(|queue| queue.account_generations.get(id).copied())
            .unwrap_or_default()
    }

    pub(crate) fn poll_gate(&self) -> Option<MutexGuard<'_, ()>> {
        self.poll_gate.lock().ok()
    }

    pub(crate) fn wait_for_work(&self, stopping: impl Fn() -> bool) -> Option<bool> {
        let mut queue = self.queue.lock().ok()?;
        loop {
            if stopping() {
                return None;
            }
            if queue.pending {
                let force = queue.force;
                queue.pending = false;
                queue.force = false;
                queue.running = true;
                return Some(force);
            }
            let (next, timeout) = self
                .wake
                .wait_timeout(queue, CATALOG_RECONCILE_INTERVAL)
                .ok()?;
            queue = next;
            if timeout.timed_out() {
                queue.running = true;
                return Some(false);
            }
        }
    }

    pub(crate) fn finish_pass(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.running = false;
            self.wake.notify_all();
        }
    }

    pub(crate) fn stop(&self) {
        self.wake.notify_all();
    }

    pub(crate) fn mark_catalog_publication_pending(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.catalog_publication_pending = true;
        }
    }

    pub(crate) fn catalog_publication_pending(&self) -> bool {
        self.queue
            .lock()
            .is_ok_and(|queue| queue.catalog_publication_pending)
    }

    pub(crate) fn catalog_publication_succeeded(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.catalog_publication_pending = false;
        }
    }

    pub(crate) fn due(
        &self,
        id: &str,
        fingerprint: &str,
        generation: u64,
        force: bool,
        now: Instant,
    ) -> bool {
        let Ok(mut queue) = self.queue.lock() else {
            return false;
        };
        let poll = queue.polls.entry(id.to_owned()).or_default();
        if poll.fingerprint != fingerprint || poll.generation != generation {
            *poll = PollState {
                fingerprint: fingerprint.to_owned(),
                generation,
                expires_at: Some(now),
                ..PollState::default()
            };
        }
        if !force && poll.retry_after.is_some_and(|retry| now < retry) {
            return false;
        }
        force || poll.expires_at.is_none_or(|expires| now >= expires)
    }

    pub(crate) fn completed(&self, id: &str, success: bool, unauthorized: bool, now: Instant) {
        if let Ok(mut queue) = self.queue.lock() {
            let Some(poll) = queue.polls.get_mut(id) else {
                return;
            };
            if success {
                poll.failures = 0;
                poll.retry_after = None;
                poll.expires_at = Some(now + CATALOG_MAX_AGE);
            } else {
                poll.failures = poll.failures.saturating_add(1);
                let delay = if unauthorized {
                    AUTH_RETRY
                } else {
                    let factor = 1_u32 << poll.failures.saturating_sub(1).min(20);
                    BASE_RETRY.saturating_mul(factor).min(MAX_RETRY)
                };
                poll.retry_after = Some(now + delay);
            }
            self.wake.notify_all();
        }
    }

    pub(crate) fn mark_fresh(&self, id: &str, fingerprint: &str, generation: u64, now: Instant) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.polls.insert(
                id.to_owned(),
                PollState {
                    fingerprint: fingerprint.to_owned(),
                    generation,
                    expires_at: Some(now + CATALOG_MAX_AGE),
                    retry_after: None,
                    failures: 0,
                },
            );
            self.wake.notify_all();
        }
    }

    pub(crate) fn retain_sources(&self, present: &std::collections::BTreeSet<String>) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.polls.retain(|id, _| present.contains(id));
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn wait_until_idle(&self, timeout: Duration) -> bool {
        let Ok(mut queue) = self.queue.lock() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        while queue.running || queue.pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let Ok((next, result)) = self.wake.wait_timeout(queue, remaining) else {
                return false;
            };
            queue = next;
            if result.timed_out() && (queue.running || queue.pending) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::CatalogRefreshState;
    use std::time::{Duration, Instant};

    #[test]
    fn authentication_failures_back_off_and_source_changes_retry_immediately() {
        let state = CatalogRefreshState::default();
        let now = Instant::now();
        assert!(state.due("account", "owner-a", 0, false, now));
        state.completed("account", false, true, now);
        assert!(!state.due(
            "account",
            "owner-a",
            0,
            false,
            now + Duration::from_secs(60)
        ));
        assert!(state.due(
            "account",
            "owner-a",
            0,
            false,
            now + Duration::from_secs(5 * 60)
        ));

        let changed_source = CatalogRefreshState::default();
        assert!(changed_source.due("account", "owner-a", 0, false, now));
        changed_source.completed("account", false, true, now);
        assert!(changed_source.due(
            "account",
            "owner-b",
            0,
            false,
            now + Duration::from_secs(60)
        ));
    }
}
