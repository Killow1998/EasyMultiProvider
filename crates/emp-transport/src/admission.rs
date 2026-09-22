use serde::Serialize;
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

pub const MAX_PROXY_REQUEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_EXPANDED_REQUEST_BYTES: usize = 1024 * 1024 * 1024;
pub const MEMORY_RESERVATION_FACTOR: usize = 8;
pub const REQUEST_GROWTH_QUANTUM: usize = 16 * 1024 * 1024;
pub const MIN_MEMORY_HEADROOM_BYTES: usize = 512 * 1024 * 1024;
const NOTICE_LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Http,
    WebSocket,
}

impl TransportKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::WebSocket => "websocket",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryStatus {
    pub available: usize,
    pub total: Option<usize>,
    pub used: Option<usize>,
    pub used_percent: Option<f64>,
}

impl MemoryStatus {
    pub fn available(available: usize) -> Self {
        Self {
            available,
            total: None,
            used: None,
            used_percent: None,
        }
    }

    fn sanitized(self) -> ObservedMemory {
        let total = self.total.unwrap_or(0);
        let used = self
            .used
            .unwrap_or_else(|| total.saturating_sub(self.available));
        let used_percent = self
            .used_percent
            .filter(|value| value.is_finite())
            .map(|value| (value.clamp(0.0, 100.0) * 10.0).round() / 10.0);
        ObservedMemory {
            available: self.available,
            total,
            used,
            used_percent,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ObservedMemory {
    available: usize,
    total: usize,
    used: usize,
    used_percent: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimitsConfig {
    pub baseline: usize,
    pub maximum: usize,
    pub growth_quantum: usize,
    pub memory_headroom: usize,
}

impl Default for RequestLimitsConfig {
    fn default() -> Self {
        Self {
            baseline: MAX_PROXY_REQUEST_BYTES,
            maximum: MAX_EXPANDED_REQUEST_BYTES,
            growth_quantum: REQUEST_GROWTH_QUANTUM,
            memory_headroom: MIN_MEMORY_HEADROOM_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestLimitsError {
    InvalidGrowthLimits,
    InvalidMemoryLimits,
    StateUnavailable,
}

impl fmt::Display for RequestLimitsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidGrowthLimits => "invalid request growth limits",
            Self::InvalidMemoryLimits => "invalid request memory limits",
            Self::StateUnavailable => "request admission state is unavailable",
        })
    }
}

impl std::error::Error for RequestLimitsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestCapacityReason {
    HardLimit,
    MemoryLimit,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestCapacityError {
    pub limit: usize,
    pub decoded: bool,
    pub reason: RequestCapacityReason,
    pub available_bytes: usize,
    pub required_memory_bytes: usize,
    pub memory_total_bytes: usize,
    pub memory_used_bytes: usize,
    pub memory_used_percent: Option<f64>,
}

impl RequestCapacityError {
    pub const fn http_status(&self) -> u16 {
        match self.reason {
            RequestCapacityReason::HardLimit => 413,
            RequestCapacityReason::MemoryLimit => 503,
        }
    }

    pub const fn websocket_close_code(&self) -> u16 {
        match self.reason {
            RequestCapacityReason::HardLimit => 1009,
            RequestCapacityReason::MemoryLimit => 1013,
        }
    }
}

impl RequestCapacityReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HardLimit => "hard_limit",
            Self::MemoryLimit => "memory_limit",
        }
    }
}

impl fmt::Display for RequestCapacityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            RequestCapacityReason::HardLimit => write!(
                formatter,
                "{}request body is too large (EMP limit: {} bytes)",
                if self.decoded { "decoded " } else { "" },
                self.limit
            ),
            RequestCapacityReason::MemoryLimit => {
                formatter.write_str(
                    "EMP out of memory safeguard blocked this request (system memory: ",
                )?;
                if let Some(used) = self.memory_used_percent {
                    write!(formatter, "{used:.1}% used, ")?;
                }
                write!(
                    formatter,
                    "{:.1} of {:.1} MiB used; {:.1} MiB available; {:.1} MiB required including safety headroom). Free memory and retry.",
                    self.memory_used_bytes as f64 / (1024.0 * 1024.0),
                    self.memory_total_bytes as f64 / (1024.0 * 1024.0),
                    self.available_bytes as f64 / (1024.0 * 1024.0),
                    self.required_memory_bytes as f64 / (1024.0 * 1024.0)
                )
            }
        }
    }
}

impl std::error::Error for RequestCapacityError {}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestLimitNotice {
    pub id: u64,
    pub timestamp: u64,
    pub kind: &'static str,
    pub transport: &'static str,
    pub previous_bytes: usize,
    pub requested_bytes: usize,
    pub target_bytes: usize,
    pub limit_bytes: usize,
    pub reservation_bytes: usize,
    pub required_memory_bytes: usize,
    pub available_bytes: usize,
    pub memory_total_bytes: usize,
    pub memory_used_bytes: usize,
    pub memory_used_percent: Option<f64>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestLimitsSnapshot {
    pub run_id: String,
    pub baseline_bytes: usize,
    pub maximum_bytes: usize,
    pub reserved_bytes: usize,
    pub notices: Vec<RequestLimitNotice>,
}

#[derive(Debug, Default)]
struct RequestLimitsState {
    reserved: usize,
    sequence: u64,
    notices: VecDeque<RequestLimitNotice>,
}

type MemoryProbe = dyn Fn() -> Option<MemoryStatus> + Send + Sync;
type Clock = dyn Fn() -> u64 + Send + Sync;

pub struct RequestLimits {
    config: RequestLimitsConfig,
    memory_status: Box<MemoryProbe>,
    clock: Box<Clock>,
    run_id: String,
    state: Mutex<RequestLimitsState>,
}

impl RequestLimits {
    pub fn new<M, C>(
        config: RequestLimitsConfig,
        memory_status: M,
        clock: C,
        run_id: impl Into<String>,
    ) -> Result<Arc<Self>, RequestLimitsError>
    where
        M: Fn() -> Option<MemoryStatus> + Send + Sync + 'static,
        C: Fn() -> u64 + Send + Sync + 'static,
    {
        if config.baseline == 0 || config.maximum < config.baseline {
            return Err(RequestLimitsError::InvalidGrowthLimits);
        }
        if config.growth_quantum == 0 {
            return Err(RequestLimitsError::InvalidMemoryLimits);
        }
        Ok(Arc::new(Self {
            config,
            memory_status: Box::new(memory_status),
            clock: Box::new(clock),
            run_id: run_id.into(),
            state: Mutex::new(RequestLimitsState::default()),
        }))
    }

    pub fn request(self: &Arc<Self>, transport: TransportKind) -> RequestBudget {
        RequestBudget {
            owner: Arc::clone(self),
            transport,
            limit: self.config.baseline,
            reserved: 0,
        }
    }

    pub fn snapshot(&self) -> Result<RequestLimitsSnapshot, RequestLimitsError> {
        let state = self.lock_state()?;
        Ok(RequestLimitsSnapshot {
            run_id: self.run_id.clone(),
            baseline_bytes: self.config.baseline,
            maximum_bytes: self.config.maximum,
            reserved_bytes: state.reserved,
            notices: state.notices.iter().cloned().collect(),
        })
    }

    fn ensure(&self, budget: &mut RequestBudget, size: usize) -> Result<(), RequestCapacityError> {
        if size <= budget.limit {
            return Ok(());
        }
        let previous = budget.limit;
        let quantized = size
            .checked_add(self.config.growth_quantum - 1)
            .map(|value| value / self.config.growth_quantum)
            .and_then(|value| value.checked_mul(self.config.growth_quantum));
        let hard_limit = size > self.config.maximum || quantized.is_none();
        let target = quantized
            .unwrap_or(self.config.maximum)
            .max(self.config.baseline)
            .min(self.config.maximum);
        let memory = if hard_limit {
            MemoryStatus::available(0).sanitized()
        } else {
            (self.memory_status)()
                .unwrap_or_else(|| MemoryStatus::available(0))
                .sanitized()
        };
        let needed = if hard_limit {
            0
        } else {
            target.saturating_mul(MEMORY_RESERVATION_FACTOR)
        };
        let mut state = self.lock_state().map_err(|_| RequestCapacityError {
            limit: target,
            decoded: false,
            reason: RequestCapacityReason::MemoryLimit,
            available_bytes: 0,
            required_memory_bytes: usize::MAX,
            memory_total_bytes: 0,
            memory_used_bytes: 0,
            memory_used_percent: None,
        })?;
        let other_reserved = state.reserved.saturating_sub(budget.reserved);
        let required = if hard_limit {
            0
        } else {
            needed
                .checked_add(other_reserved)
                .and_then(|value| value.checked_add(self.config.memory_headroom))
                .unwrap_or(usize::MAX)
        };
        let reason = if hard_limit {
            Some(RequestCapacityReason::HardLimit)
        } else if required > memory.available {
            Some(RequestCapacityReason::MemoryLimit)
        } else {
            None
        };
        if reason.is_none() {
            state.reserved = other_reserved + needed;
            budget.reserved = needed;
            budget.limit = target;
        }
        state.sequence = state.sequence.saturating_add(1);
        let notice = RequestLimitNotice {
            id: state.sequence,
            timestamp: (self.clock)(),
            kind: if reason.is_some() {
                "blocked"
            } else {
                "expanded"
            },
            transport: budget.transport.as_str(),
            previous_bytes: previous,
            requested_bytes: size,
            target_bytes: if reason == Some(RequestCapacityReason::HardLimit) {
                self.config.maximum
            } else {
                target
            },
            limit_bytes: if reason == Some(RequestCapacityReason::HardLimit) {
                self.config.maximum
            } else {
                budget.limit
            },
            reservation_bytes: needed,
            required_memory_bytes: required,
            available_bytes: memory.available,
            memory_total_bytes: memory.total,
            memory_used_bytes: memory.used,
            memory_used_percent: memory.used_percent,
            reason: reason.map_or("", RequestCapacityReason::as_str),
        };
        if state.notices.len() == NOTICE_LIMIT {
            state.notices.pop_front();
        }
        state.notices.push_back(notice);
        if let Some(reason) = reason {
            return Err(RequestCapacityError {
                limit: if reason == RequestCapacityReason::HardLimit {
                    self.config.maximum
                } else {
                    target
                },
                decoded: false,
                reason,
                available_bytes: memory.available,
                required_memory_bytes: required,
                memory_total_bytes: memory.total,
                memory_used_bytes: memory.used,
                memory_used_percent: memory.used_percent,
            });
        }
        Ok(())
    }

    fn release(&self, reserved: usize) {
        if reserved == 0 {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.reserved = state.reserved.saturating_sub(reserved);
        }
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, RequestLimitsState>, RequestLimitsError> {
        self.state
            .lock()
            .map_err(|_| RequestLimitsError::StateUnavailable)
    }
}

pub struct RequestBudget {
    owner: Arc<RequestLimits>,
    transport: TransportKind,
    limit: usize,
    reserved: usize,
}

impl RequestBudget {
    pub const fn limit(&self) -> usize {
        self.limit
    }

    pub const fn reserved(&self) -> usize {
        self.reserved
    }

    pub fn ensure(&mut self, size: usize) -> Result<(), RequestCapacityError> {
        let owner = Arc::clone(&self.owner);
        owner.ensure(self, size)
    }

    pub fn release(mut self) {
        self.owner.release(self.reserved);
        self.reserved = 0;
    }
}

impl Drop for RequestBudget {
    fn drop(&mut self) {
        self.owner.release(self.reserved);
        self.reserved = 0;
    }
}
