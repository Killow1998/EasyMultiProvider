//! Limits and accounting for reusable idle connections.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionPoolPolicy {
    pub max_idle_per_route: usize,
    pub max_idle_total: usize,
    pub idle_timeout: Duration,
}

impl Default for ConnectionPoolPolicy {
    fn default() -> Self {
        Self {
            max_idle_per_route: DEFAULT_MAX_IDLE_PER_ROUTE,
            max_idle_total: DEFAULT_MAX_IDLE_TOTAL,
            idle_timeout: DEFAULT_IDLE_CONNECTION_TIMEOUT,
        }
    }
}

impl ConnectionPoolPolicy {
    pub fn validate(&self) -> HttpResult<()> {
        if self.max_idle_per_route == 0
            || self.max_idle_total < self.max_idle_per_route
            || self.idle_timeout.is_zero()
        {
            return Err(HttpClientPolicyError::InvalidPoolKey);
        }
        Ok(())
    }
}
