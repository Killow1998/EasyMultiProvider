use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, TryLockError,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[cfg(test)]
type TestClock = Arc<dyn Fn() -> Instant + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotLimits {
    pub(crate) initial: usize,
    pub(crate) maximum: usize,
    pub(crate) growth: usize,
}

impl SlotLimits {
    const fn new(initial: usize, maximum: usize, growth: usize) -> Self {
        Self {
            initial,
            maximum,
            growth,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectionAdmissionConfig {
    pub(crate) requests: SlotLimits,
    pub(crate) websockets: SlotLimits,
}

impl Default for ConnectionAdmissionConfig {
    fn default() -> Self {
        Self {
            requests: SlotLimits::new(64, 256, 32),
            websockets: SlotLimits::new(48, 224, 32),
        }
    }
}

pub(crate) struct ConnectionAdmission {
    gate: Mutex<()>,
    work_finished: Arc<Notify>,
    requests: Arc<AdaptiveSlotPool>,
    websockets: Arc<AdaptiveSlotPool>,
}

impl ConnectionAdmission {
    pub(crate) fn new(config: ConnectionAdmissionConfig) -> Self {
        let work_finished = Arc::new(Notify::new());
        Self {
            gate: Mutex::new(()),
            requests: Arc::new(AdaptiveSlotPool::new(
                config.requests,
                work_finished.clone(),
            )),
            websockets: Arc::new(AdaptiveSlotPool::new(
                config.websockets,
                work_finished.clone(),
            )),
            work_finished,
        }
    }

    pub(crate) fn acquire_request(&self) -> Option<ConnectionPermit> {
        AdaptiveSlotPool::acquire(&self.requests)
    }

    pub(crate) fn acquire_websocket(&self) -> Option<ConnectionPermit> {
        AdaptiveSlotPool::acquire(&self.websockets)
    }

    pub(crate) fn is_quiescing(&self) -> bool {
        self.requests
            .state
            .lock()
            .map_or(true, |state| state.closed)
    }

    pub(crate) async fn wait_for_websocket_drain(&self) {
        let notified = self.websockets.closing.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self
            .websockets
            .state
            .lock()
            .map_or(true, |state| state.closed)
        {
            notified.await;
        }
    }

    /// Exit cannot wait behind a management request's drain guard. Closing
    /// each pool permanently also prevents that guard from reopening it.
    pub(crate) fn close(&self) {
        self.requests.close_permanently();
        self.websockets.close_permanently();
    }

    pub(crate) fn has_business_work(&self) -> bool {
        let requests = self.requests.state.lock().expect("request admission");
        let websockets = self.websockets.state.lock().expect("WebSocket admission");
        requests.working > 0 || websockets.working > 0
    }

    pub(crate) fn business_crossed(&self, deadline: Instant) -> bool {
        let requests = self.requests.state.lock().expect("request admission");
        let websockets = self.websockets.state.lock().expect("WebSocket admission");
        [&*requests, &*websockets]
            .into_iter()
            .any(|state| state.working > 0 || state.idle_at.is_some_and(|at| at > deadline))
    }

    #[cfg(test)]
    pub(crate) fn set_test_clock(&self, now: TestClock) {
        assert!(self.requests.test_clock.set(now.clone()).is_ok());
        assert!(self.websockets.test_clock.set(now).is_ok());
    }

    pub(crate) async fn wait_for_business_idle(&self) {
        loop {
            let notified = self.work_finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.has_business_work() {
                return;
            }
            notified.await;
        }
    }

    /// Stop new work and wait until admitted requests and WebSockets drain.
    /// `exempt_requests` accounts for the management request doing a restore.
    pub(crate) fn quiesce(
        &self,
        exempt_requests: usize,
        timeout: Duration,
    ) -> Option<AdmissionGateGuard<'_>> {
        self.quiesce_with(exempt_requests, timeout, |slots| slots.active)
    }

    pub(crate) fn quiesce_work(
        &self,
        exempt_requests: usize,
        timeout: Duration,
    ) -> Option<AdmissionGateGuard<'_>> {
        self.quiesce_with(exempt_requests, timeout, |slots| slots.working)
    }

    fn quiesce_with(
        &self,
        exempt_requests: usize,
        timeout: Duration,
        occupied: impl Fn(&SlotState) -> usize,
    ) -> Option<AdmissionGateGuard<'_>> {
        let owner = match self.gate.try_lock() {
            Ok(owner) => owner,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let guard = AdmissionGateGuard {
            _owner: owner,
            requests: Arc::clone(&self.requests),
            websockets: Arc::clone(&self.websockets),
        };
        if !guard.requests.close() || !guard.websockets.close() {
            return None;
        }
        let deadline = Instant::now().checked_add(timeout)?;
        if !guard
            .requests
            .wait_for_at_most(exempt_requests, deadline, &occupied)
            || !guard.websockets.wait_for_at_most(0, deadline, occupied)
        {
            return None;
        }
        Some(guard)
    }

    #[cfg(test)]
    pub(crate) fn active_requests(&self) -> usize {
        self.requests.active()
    }

    #[cfg(test)]
    pub(crate) fn active_websockets(&self) -> usize {
        self.websockets.active()
    }

    #[cfg(test)]
    pub(crate) fn wait_for_websocket_release(&self, remaining: usize, timeout: Duration) -> bool {
        self.websockets
            .wait_for_at_most(remaining, Instant::now() + timeout, |slots| slots.active)
    }
}

#[derive(Default)]
struct SlotState {
    active: usize,
    working: usize,
    idle_at: Option<Instant>,
    capacity: usize,
    maximum: usize,
    growth: usize,
    closed: bool,
    permanently_closed: bool,
}

struct AdaptiveSlotPool {
    state: Mutex<SlotState>,
    drained: Condvar,
    closing: Notify,
    work_finished: Arc<Notify>,
    #[cfg(test)]
    test_clock: std::sync::OnceLock<TestClock>,
}

impl AdaptiveSlotPool {
    fn new(limits: SlotLimits, work_finished: Arc<Notify>) -> Self {
        let capacity = limits.initial.max(1);
        Self {
            state: Mutex::new(SlotState {
                capacity,
                maximum: limits.maximum.max(capacity),
                growth: limits.growth.max(1),
                ..SlotState::default()
            }),
            drained: Condvar::new(),
            closing: Notify::new(),
            work_finished,
            #[cfg(test)]
            test_clock: std::sync::OnceLock::new(),
        }
    }

    fn acquire(pool: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut state = pool.state.lock().ok()?;
        if state.closed {
            return None;
        }
        if state.active >= state.capacity {
            if state.capacity >= state.maximum {
                return None;
            }
            state.capacity = state
                .capacity
                .saturating_add(state.growth)
                .min(state.maximum);
        }
        state.active += 1;
        state.working += 1;
        Some(ConnectionPermit(Some(Arc::new(ConnectionSlot {
            pool: Arc::clone(pool),
            holders: AtomicUsize::new(1),
        }))))
    }

    fn close(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        state.closed = true;
        self.closing.notify_waiters();
        true
    }

    fn reopen(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = state.permanently_closed;
            self.drained.notify_all();
        }
    }

    fn close_permanently(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.permanently_closed = true;
        self.closing.notify_waiters();
    }

    fn wait_for_at_most(
        &self,
        maximum_active: usize,
        deadline: Instant,
        active: impl Fn(&SlotState) -> usize,
    ) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        while active(&state) > maximum_active {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let Ok((next, timeout)) = self.drained.wait_timeout(state, remaining) else {
                return false;
            };
            state = next;
            if timeout.timed_out() && active(&state) > maximum_active {
                return false;
            }
        }
        true
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.state.lock().map(|state| state.active).unwrap_or(0)
    }
}

pub(crate) struct AdmissionGateGuard<'a> {
    _owner: MutexGuard<'a, ()>,
    requests: Arc<AdaptiveSlotPool>,
    websockets: Arc<AdaptiveSlotPool>,
}

impl AdmissionGateGuard<'_> {
    /// Keep the admission gate shut for process exit.
    pub(crate) fn keep_closed(self) {
        self.requests.close_permanently();
        self.websockets.close_permanently();
    }
}

impl Drop for AdmissionGateGuard<'_> {
    fn drop(&mut self) {
        self.requests.reopen();
        self.websockets.reopen();
    }
}

pub(crate) struct ConnectionPermit(Option<Arc<ConnectionSlot>>);

impl ConnectionPermit {
    pub(crate) fn slot(&self) -> &Arc<ConnectionSlot> {
        self.0.as_ref().expect("active business permit")
    }

    pub(crate) fn finish_with<T>(mut self, handoff: impl FnOnce() -> T) -> T {
        self.release(handoff)
    }

    fn release<T>(&mut self, handoff: impl FnOnce() -> T) -> T {
        let slot = self.0.take();
        if let Some(slot) = &slot
            && let Ok(mut state) = slot.pool.state.lock()
        {
            if slot.holders.fetch_sub(1, Ordering::Relaxed) == 1 {
                state.working -= 1;
                if state.working == 0 {
                    #[cfg(test)]
                    let now = slot
                        .pool
                        .test_clock
                        .get()
                        .map_or_else(Instant::now, |now| now());
                    #[cfg(not(test))]
                    let now = Instant::now();
                    state.idle_at = Some(now);
                    slot.pool.work_finished.notify_waiters();
                }
                slot.pool.drained.notify_all();
            }
            return handoff();
        }
        handoff()
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.release(|| ());
    }
}

pub(crate) struct ConnectionSlot {
    pool: Arc<AdaptiveSlotPool>,
    holders: AtomicUsize,
}

impl ConnectionSlot {
    pub(crate) fn start_work(self: &Arc<Self>) -> ConnectionPermit {
        let mut state = self.pool.state.lock().expect("connection admission");
        if self.holders.fetch_add(1, Ordering::Relaxed) == 0 {
            state.working += 1;
        }
        ConnectionPermit(Some(self.clone()))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        if let Ok(mut state) = self.pool.state.lock() {
            state.active -= 1;
            self.pool.drained.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_pool_expands_by_growth_step_and_reuses_released_capacity() {
        let pool = Arc::new(AdaptiveSlotPool::new(
            SlotLimits {
                initial: 2,
                maximum: 4,
                growth: 1,
            },
            Arc::new(Notify::new()),
        ));
        let first = AdaptiveSlotPool::acquire(&pool).expect("first slot");
        let second = AdaptiveSlotPool::acquire(&pool).expect("second slot");
        assert_eq!(pool.state.lock().unwrap().capacity, 2);
        let third = AdaptiveSlotPool::acquire(&pool).expect("pool grows to three");
        assert_eq!(pool.state.lock().unwrap().capacity, 3);
        let fourth = AdaptiveSlotPool::acquire(&pool).expect("fourth slot");
        assert_eq!(pool.state.lock().unwrap().capacity, 4);
        assert!(AdaptiveSlotPool::acquire(&pool).is_none());

        drop(second);
        assert!(AdaptiveSlotPool::acquire(&pool).is_some());
        drop((first, third, fourth));
    }

    #[test]
    fn quiesce_closes_new_requests_and_reopens_when_the_guard_is_dropped() {
        let admission = ConnectionAdmission::new(ConnectionAdmissionConfig::default());
        let current = admission.acquire_request().expect("current request");
        let gate = admission
            .quiesce(1, Duration::ZERO)
            .expect("only the restore request remains");
        assert!(admission.acquire_request().is_none());
        drop(gate);
        assert!(admission.acquire_request().is_some());
        drop(current);
    }

    #[test]
    fn overlapping_quiesce_cannot_reopen_the_first_gate() {
        let admission = ConnectionAdmission::new(ConnectionAdmissionConfig::default());
        let first = admission
            .quiesce(0, Duration::ZERO)
            .expect("empty pools drain");
        assert!(admission.quiesce(0, Duration::ZERO).is_none());
        assert!(admission.acquire_request().is_none());
        drop(first);
        assert!(admission.acquire_request().is_some());
    }

    #[test]
    fn rejected_overlapping_quiesce_stays_closed_until_admitted_work_drains() {
        use std::sync::mpsc;
        use std::thread;

        let admission = Arc::new(ConnectionAdmission::new(
            ConnectionAdmissionConfig::default(),
        ));
        let first_restore = admission.acquire_request().expect("first restore request");
        let rejected_restore = admission.acquire_request().expect("second restore request");
        let worker_admission = Arc::clone(&admission);
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let gate = worker_admission
                .quiesce(1, Duration::from_secs(2))
                .expect("first restore waits for the other admitted request");
            ready_tx.send(()).expect("test is waiting for the gate");
            release_rx.recv().expect("test releases the gate");
            drop(gate);
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if admission.acquire_request().is_none() {
                break;
            }
            assert!(Instant::now() < deadline, "quiesce did not close admission");
            thread::yield_now();
        }

        assert!(admission.quiesce(1, Duration::ZERO).is_none());
        drop(rejected_restore);
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the first restore proceeds after the other request releases");
        assert!(admission.acquire_request().is_none());
        release_tx.send(()).expect("gate worker is still waiting");
        worker.join().expect("gate worker completes");
        assert!(admission.acquire_request().is_some());
        drop(first_restore);
    }

    #[test]
    fn quiesce_timeout_reopens_admission_without_restoring_configuration() {
        let admission = ConnectionAdmission::new(ConnectionAdmissionConfig::default());
        let active = admission.acquire_request().expect("active request");
        assert!(admission.quiesce(0, Duration::ZERO).is_none());
        assert!(admission.acquire_request().is_some());
        drop(active);
        let gate = admission
            .quiesce(0, Duration::ZERO)
            .expect("released requests allow the timed out restore to retry");
        assert!(admission.acquire_request().is_none());
        drop(gate);
        assert!(admission.acquire_request().is_some());
    }

    #[test]
    fn quit_can_leave_admission_permanently_closed() {
        let admission = ConnectionAdmission::new(ConnectionAdmissionConfig::default());
        admission
            .quiesce(0, Duration::ZERO)
            .expect("empty pools drain")
            .keep_closed();
        assert!(admission.acquire_request().is_none());
        assert!(admission.acquire_websocket().is_none());
        let shutdown_gate = admission
            .quiesce(0, Duration::ZERO)
            .expect("shutdown can wait on a permanently closed gate");
        drop(shutdown_gate);
        assert!(admission.acquire_request().is_none());
    }

    #[test]
    fn exit_closes_admission_even_while_a_restore_guard_is_held() {
        let admission = ConnectionAdmission::new(ConnectionAdmissionConfig::default());
        let current = admission.acquire_request().unwrap();
        let restore = admission.quiesce(1, Duration::ZERO).unwrap();
        admission.close();
        drop(restore);
        drop(current);
        assert!(admission.acquire_request().is_none());
        assert!(admission.acquire_websocket().is_none());
    }
}
