use super::CatalogRefreshState;
use std::time::{Duration, Instant};

#[test]
fn successful_refresh_waits_a_day_unless_forced_or_the_source_changes() {
    let state = CatalogRefreshState::default();
    let now = Instant::now();
    assert!(state.due("account", "owner-a", 0, false, now));
    state.completed("account", true, false, now);
    let day = Duration::from_secs(86_400);
    let before_expiry = now + day - Duration::from_secs(1);
    assert!(!state.due("account", "owner-a", 0, false, before_expiry));
    assert!(state.due("account", "owner-a", 0, true, before_expiry));
    assert!(state.due("account", "owner-a", 0, false, now + day));
    state.mark_fresh("account", "owner-a", 0, now + day);
    assert!(!state.due("account", "owner-a", 0, false, now + day));
    assert!(state.due("account", "owner-b", 0, false, now + day));
}

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

#[cfg(unix)]
impl super::super::CatalogState {
    pub(crate) fn wait_for_refresh_request(&self, stopping: impl Fn() -> bool) -> Option<bool> {
        self.refresh.wait_for_work(stopping)
    }

    pub(crate) fn finish_refresh_pass(&self) {
        self.refresh.finish_pass();
    }

    pub(crate) fn wait_until_idle(&self, timeout: Duration) -> bool {
        let Ok(mut queue) = self.refresh.queue.lock() else {
            return false;
        };
        let deadline = Instant::now() + timeout;
        while queue.running || queue.pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let Ok((next, result)) = self.refresh.wake.wait_timeout(queue, remaining) else {
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
