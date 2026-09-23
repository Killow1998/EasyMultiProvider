use std::sync::{Arc, Mutex};

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
    requests: Arc<AdaptiveSlotPool>,
    websockets: Arc<AdaptiveSlotPool>,
}

impl ConnectionAdmission {
    pub(crate) fn new(config: ConnectionAdmissionConfig) -> Self {
        Self {
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
struct SlotState {
    active: usize,
    capacity: usize,
    maximum: usize,
    growth: usize,
}

struct AdaptiveSlotPool {
    state: Mutex<SlotState>,
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
            }),
        }
    }

    fn acquire(pool: &Arc<Self>) -> Option<ConnectionPermit> {
        let mut state = pool.state.lock().ok()?;
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
        }
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.state.lock().map(|state| state.active).unwrap_or(0)
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
