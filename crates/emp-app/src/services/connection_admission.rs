use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

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
    gate: Mutex<GateOwnership>,
    requests: Arc<AdaptiveSlotPool>,
    websockets: Arc<AdaptiveSlotPool>,
}

impl ConnectionAdmission {
    pub(crate) fn new(config: ConnectionAdmissionConfig) -> Self {
        Self {
            gate: Mutex::new(GateOwnership::default()),
            requests: Arc::new(AdaptiveSlotPool::new(config.requests)),
            websockets: Arc::new(AdaptiveSlotPool::new(config.websockets)),
        }
    }

    pub(crate) fn acquire_request(&self) -> Option<ConnectionPermit> {
        AdaptiveSlotPool::acquire(&self.requests)
    }

    pub(crate) fn acquire_websocket(&self) -> Option<ConnectionPermit> {
        AdaptiveSlotPool::acquire(&self.websockets)
    }

    /// Stop new work and wait until admitted requests and WebSockets drain.
    /// `exempt_requests` accounts for the management request doing a restore.
    pub(crate) fn quiesce(
        &self,
        exempt_requests: usize,
        timeout: Duration,
    ) -> Option<AdmissionGateGuard<'_>> {
        let owner = match self.gate.try_lock() {
            Ok(owner) => owner,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let reopen_on_drop = !owner.permanently_closed;
        let guard = AdmissionGateGuard {
            owner,
            requests: Arc::clone(&self.requests),
            websockets: Arc::clone(&self.websockets),
            reopen_on_drop,
        };
        if !guard.requests.close() || !guard.websockets.close() {
            return None;
        }
        let deadline = Instant::now().checked_add(timeout)?;
        if !guard.requests.wait_for_at_most(exempt_requests, deadline)
            || !guard.websockets.wait_for_at_most(0, deadline)
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
}

#[derive(Default)]
struct GateOwnership {
    permanently_closed: bool,
}

#[derive(Default)]
struct SlotState {
    active: usize,
    capacity: usize,
    maximum: usize,
    growth: usize,
    closed: bool,
}

struct AdaptiveSlotPool {
    state: Mutex<SlotState>,
    drained: Condvar,
}

impl AdaptiveSlotPool {
    fn new(limits: SlotLimits) -> Self {
        let capacity = limits.initial.max(1);
        Self {
            state: Mutex::new(SlotState {
                active: 0,
                capacity,
                maximum: limits.maximum.max(capacity),
                growth: limits.growth.max(1),
                closed: false,
            }),
            drained: Condvar::new(),
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
        Some(ConnectionPermit {
            pool: Arc::clone(pool),
        })
    }

    fn release(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = state
                .active
                .checked_sub(1)
                .expect("connection admission permit released exactly once");
            self.drained.notify_all();
        }
    }

    fn close(&self) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        state.closed = true;
        true
    }

    fn reopen(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = false;
            self.drained.notify_all();
        }
    }

    fn wait_for_at_most(&self, maximum_active: usize, deadline: Instant) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        while state.active > maximum_active {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let Ok((next, timeout)) = self.drained.wait_timeout(state, remaining) else {
                return false;
            };
            state = next;
            if timeout.timed_out() && state.active > maximum_active {
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
    owner: MutexGuard<'a, GateOwnership>,
    requests: Arc<AdaptiveSlotPool>,
    websockets: Arc<AdaptiveSlotPool>,
    reopen_on_drop: bool,
}

impl AdmissionGateGuard<'_> {
    /// Keep the admission gate shut after a successful native restore.
    pub(crate) fn keep_closed(mut self) {
        self.owner.permanently_closed = true;
        self.reopen_on_drop = false;
    }
}

impl Drop for AdmissionGateGuard<'_> {
    fn drop(&mut self) {
        if self.reopen_on_drop {
            self.requests.reopen();
            self.websockets.reopen();
        }
    }
}

pub(crate) struct ConnectionPermit {
    pool: Arc<AdaptiveSlotPool>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.pool.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_pool_expands_by_growth_step_and_reuses_released_capacity() {
        let pool = Arc::new(AdaptiveSlotPool::new(SlotLimits {
            initial: 2,
            maximum: 4,
            growth: 1,
        }));
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
    fn successful_restore_can_leave_admission_closed() {
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
    fn production_limits_match_python_listener_defaults() {
        let config = ConnectionAdmissionConfig::default();
        assert_eq!(
            (
                config.requests.initial,
                config.requests.maximum,
                config.requests.growth
            ),
            (64, 256, 32)
        );
        assert_eq!(
            (
                config.websockets.initial,
                config.websockets.maximum,
                config.websockets.growth
            ),
            (48, 224, 32)
        );
    }
}
