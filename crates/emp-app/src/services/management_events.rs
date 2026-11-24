use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::{sync::watch, time::timeout};

pub(crate) const SUBSCRIBER_LIMIT: usize = 4;

pub(crate) enum Change {
    Quota,
    Activity,
    Usage,
    Integration,
    Quit,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Revisions {
    quota: u64,
    activity: u64,
    usage: u64,
    integration: u64,
    quit: u64,
}

#[derive(Default)]
struct State {
    revisions: Revisions,
    closed: bool,
}

#[derive(Default)]
pub(crate) struct ManagementEvents {
    state: watch::Sender<State>,
    subscribers: AtomicUsize,
}

pub(crate) struct Changes {
    pub(crate) quota: bool,
    pub(crate) activity: bool,
    pub(crate) usage: bool,
    pub(crate) integration: bool,
    pub(crate) quit: bool,
}

pub(crate) struct Subscription {
    feed: std::sync::Arc<ManagementEvents>,
    state: watch::Receiver<State>,
    observed: Option<Revisions>,
}

impl ManagementEvents {
    /// Publish after committing the corresponding state. Multiple changes may
    /// coalesce; subscribers read current state rather than replaying mutations.
    pub(crate) fn publish(&self, change: Change) {
        self.state.send_if_modified(|state| {
            if state.closed {
                return false;
            }
            let revision = match change {
                Change::Quota => &mut state.revisions.quota,
                Change::Activity => &mut state.revisions.activity,
                Change::Usage => &mut state.revisions.usage,
                Change::Integration => &mut state.revisions.integration,
                Change::Quit => &mut state.revisions.quit,
            };
            *revision = revision.wrapping_add(1);
            true
        });
    }

    pub(crate) fn subscribe(self: &std::sync::Arc<Self>) -> Option<Subscription> {
        self.subscribers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < SUBSCRIBER_LIMIT).then_some(count + 1)
            })
            .ok()?;
        Some(Subscription {
            feed: self.clone(),
            state: self.state.subscribe(),
            observed: None,
        })
    }

    pub(crate) fn close(&self) {
        self.state.send_modify(|state| state.closed = true);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers.load(Ordering::Acquire)
    }
}

impl Subscription {
    /// First call requests all snapshots; timeouts return no changes and closing drains a pending Quit.
    /// Waits compare revisions, so changes published before the call are still observed.
    pub(crate) async fn next(&mut self, interval: Duration) -> Option<Changes> {
        let observed = self.observed;
        let _ = timeout(
            interval,
            self.state
                .wait_for(|state| state.closed || observed != Some(state.revisions)),
        )
        .await;
        let state = self.state.borrow_and_update();
        let quit = self.observed.unwrap_or_default().quit != state.revisions.quit;
        if state.closed && !quit {
            return None;
        }
        let current = state.revisions;
        let changes = Changes {
            quit,
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

impl Drop for Subscription {
    fn drop(&mut self) {
        self.feed.subscribers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emp_test_support::{Deadline, test_timeout};
    use std::sync::{Arc, mpsc};

    #[tokio::test]
    async fn subscriber_coalesces_changes_and_releases_capacity() {
        let feed = Arc::new(ManagementEvents::default());
        let mut subscriptions: Vec<_> = (0..SUBSCRIBER_LIMIT)
            .map(|_| feed.subscribe().unwrap())
            .collect();
        assert!(feed.subscribe().is_none());
        let mut subscription = subscriptions.pop().unwrap();
        let first = subscription.next(Duration::ZERO).await.unwrap();
        assert!(first.quota && first.activity && first.usage && first.integration);
        let unchanged = subscription.next(Duration::ZERO).await.unwrap();
        assert!(
            !unchanged.quota && !unchanged.activity && !unchanged.usage && !unchanged.integration
        );
        feed.publish(Change::Quota);
        feed.publish(Change::Quota);
        feed.publish(Change::Usage);
        let changed = subscription.next(Duration::ZERO).await.unwrap();
        assert!(changed.quota && changed.usage && !changed.activity && !changed.integration);
        subscriptions.pop();
        assert!(feed.subscribe().is_some());
        feed.publish(Change::Quit);
        feed.close();
        assert!(subscription.next(Duration::ZERO).await.unwrap().quit);
        assert!(subscription.next(Duration::ZERO).await.is_none());
    }

    #[test]
    fn pending_or_waiting_subscriber_wakes_on_change_and_close() {
        let feed = Arc::new(ManagementEvents::default());
        let waiting = Arc::clone(&feed);
        let (ready, began) = mpsc::channel();
        let (sent, received) = mpsc::channel();
        let (probe, observation) = mpsc::channel::<Deadline>();
        let worker = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(async {
                    let mut subscription = waiting.subscribe().unwrap();
                    subscription.next(Duration::ZERO).await.unwrap();
                    ready.send(()).unwrap();
                    let deadline = observation.recv().unwrap();
                    let change = subscription.next(deadline.remaining()).await.unwrap();
                    sent.send(deadline.accept(change.activity, "activity publication"))
                        .unwrap();
                    let deadline = observation.recv().unwrap();
                    assert!(subscription.next(deadline.remaining()).await.is_none());
                });
        });
        began.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Deadline::after(test_timeout(Duration::from_secs(1)));
        probe.send(deadline).unwrap();
        feed.publish(Change::Activity);
        let changed = received.recv_timeout(deadline.remaining());
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        probe.send(deadline).unwrap();
        feed.close();
        worker.join().unwrap();
        assert!(changed.unwrap().unwrap());
    }
}
