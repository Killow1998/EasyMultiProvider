//! Coalesced management state changes, separate from command results/receipts.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub(crate) const SUBSCRIBER_LIMIT: usize = 4;

pub(crate) enum Change {
    Quota,
    Activity,
    Usage,
    Integration,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Revisions {
    quota: u64,
    activity: u64,
    usage: u64,
    integration: u64,
}

#[derive(Default)]
struct State {
    revisions: Revisions,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct ManagementEvents {
    state: Mutex<State>,
    wake: Condvar,
    subscribers: AtomicUsize,
}

pub(crate) struct Changes {
    pub(crate) quota: bool,
    pub(crate) activity: bool,
    pub(crate) usage: bool,
    pub(crate) integration: bool,
}

pub(crate) struct Subscription<'a> {
    feed: &'a ManagementEvents,
    observed: Option<Revisions>,
}

impl ManagementEvents {
    /// Publish after committing the corresponding state. Multiple changes may
    /// coalesce; subscribers read current state rather than replaying mutations.
    pub(crate) fn publish(&self, change: Change) {
        if let Ok(mut state) = self.state.lock() {
            if state.closed {
                return;
            }
            let revision = match change {
                Change::Quota => &mut state.revisions.quota,
                Change::Activity => &mut state.revisions.activity,
                Change::Usage => &mut state.revisions.usage,
                Change::Integration => &mut state.revisions.integration,
            };
            *revision = revision.wrapping_add(1);
            self.wake.notify_all();
        }
    }

    pub(crate) fn subscribe(&self) -> Option<Subscription<'_>> {
        self.subscribers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < SUBSCRIBER_LIMIT).then_some(count + 1)
            })
            .ok()?;
        Some(Subscription {
            feed: self,
            observed: None,
        })
    }

    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.wake.notify_all();
        }
    }
}

impl Subscription<'_> {
    /// The first call requests all snapshots. A timeout returns no changes;
    /// closing the feed ends the subscription. Checking and waiting share the
    /// publisher's lock, including changes arriving before this call.
    pub(crate) fn next(&mut self, timeout: Duration) -> Option<Changes> {
        let state = self.feed.state.lock().ok()?;
        let (state, _) = self
            .feed
            .wake
            .wait_timeout_while(state, timeout, |state| {
                !state.closed && self.observed == Some(state.revisions)
            })
            .ok()?;
        if state.closed {
            return None;
        }
        let current = state.revisions;
        let changes = Changes {
            quota: self.observed.is_none_or(|old| old.quota != current.quota),
            activity: self
                .observed
                .is_none_or(|old| old.activity != current.activity),
            usage: self.observed.is_none_or(|old| old.usage != current.usage),
            integration: self
                .observed
                .is_none_or(|old| old.integration != current.integration),
        };
        self.observed = Some(current);
        Some(changes)
    }
}

impl Drop for Subscription<'_> {
    fn drop(&mut self) {
        self.feed.subscribers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    #[test]
    fn subscriber_coalesces_changes_and_releases_capacity() {
        let feed = ManagementEvents::default();
        let mut subscriptions: Vec<_> = (0..SUBSCRIBER_LIMIT)
            .map(|_| feed.subscribe().unwrap())
            .collect();
        assert!(feed.subscribe().is_none());
        let subscription = &mut subscriptions[0];
        let first = subscription.next(Duration::ZERO).unwrap();
        assert!(first.quota && first.activity && first.usage && first.integration);
        let unchanged = subscription.next(Duration::ZERO).unwrap();
        assert!(
            !unchanged.quota && !unchanged.activity && !unchanged.usage && !unchanged.integration
        );
        feed.publish(Change::Quota);
        feed.publish(Change::Quota);
        feed.publish(Change::Usage);
        let changed = subscription.next(Duration::ZERO).unwrap();
        assert!(changed.quota && changed.usage && !changed.activity && !changed.integration);
        subscriptions.pop();
        assert!(feed.subscribe().is_some());
    }

    #[test]
    fn pending_or_waiting_subscriber_wakes_on_change_and_close() {
        let feed = Arc::new(ManagementEvents::default());
        let waiting = Arc::clone(&feed);
        let (ready, began) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut subscription = waiting.subscribe().unwrap();
            subscription.next(Duration::ZERO).unwrap();
            ready.send(()).unwrap();
            let change = subscription.next(Duration::from_secs(5)).unwrap();
            sent.send(change.activity).unwrap();
            assert!(subscription.next(Duration::from_secs(5)).is_none());
        });
        began.recv_timeout(Duration::from_secs(1)).unwrap();
        feed.publish(Change::Activity);
        let changed = received.recv_timeout(Duration::from_secs(1));
        feed.close();
        worker.join().unwrap();
        assert!(changed.unwrap());
    }
}
