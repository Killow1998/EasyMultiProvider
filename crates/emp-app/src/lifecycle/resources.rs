//! Size connection admission against this process's descriptor budget, not the
//! operating system's global limits. Never lower an inherited limit.
use crate::services::connection_admission::ConnectionAdmissionConfig;
use std::sync::OnceLock;

#[cfg(unix)]
type FileLimit = libc::rlim_t;
#[cfg(not(unix))]
type FileLimit = u64;

#[cfg(unix)]
const PREFERRED_LIMIT: FileLimit = 4096;
const RESERVED_DESCRIPTORS: FileLimit = 64;
// Both admission pools can be full. Allow four descriptors per slot in each
// pool (downstream, upstream and cancellation/monitor clones), plus the reserve.
const DESCRIPTORS_PER_POOL_SLOT: FileLimit = 8;

#[derive(Clone, Copy)]
pub(super) struct DescriptorBudget {
    pub(super) before: Option<FileLimit>,
    pub(super) soft: Option<FileLimit>,
    pub(super) raised: bool,
    pub(super) os_error: Option<i32>,
}

impl DescriptorBudget {
    pub(super) fn admission(
        self,
        mut config: ConnectionAdmissionConfig,
    ) -> ConnectionAdmissionConfig {
        if let Some(limit) = self.soft {
            let maximum = (limit.saturating_sub(RESERVED_DESCRIPTORS) / DESCRIPTORS_PER_POOL_SLOT)
                .max(1)
                .min(usize::MAX as FileLimit) as usize;
            for pool in [&mut config.requests, &mut config.websockets] {
                pool.maximum = pool.maximum.min(maximum).max(1);
                pool.initial = pool.initial.min(pool.maximum).max(1);
            }
        }
        config
    }
}

pub(super) fn prepare() -> DescriptorBudget {
    static BUDGET: OnceLock<DescriptorBudget> = OnceLock::new();
    *BUDGET.get_or_init(prepare_once)
}

#[cfg(unix)]
fn prepare_once() -> DescriptorBudget {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        // If querying fails, use a conservative admission budget rather than
        // assuming unlimited descriptors. Startup remains available.
        return DescriptorBudget {
            before: None,
            soft: Some(256),
            raised: false,
            os_error: std::io::Error::last_os_error().raw_os_error(),
        };
    }
    let finite = |value| (value != libc::RLIM_INFINITY).then_some(value);
    let before = finite(limit.rlim_cur);
    let target = if limit.rlim_max == libc::RLIM_INFINITY {
        PREFERRED_LIMIT
    } else {
        PREFERRED_LIMIT.min(limit.rlim_max)
    };
    let mut raised = false;
    let mut os_error = None;
    if limit.rlim_cur != libc::RLIM_INFINITY && limit.rlim_cur < target {
        let wanted = libc::rlimit {
            rlim_cur: target,
            rlim_max: limit.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &wanted) } == 0 {
            limit = wanted;
            raised = true;
        } else {
            os_error = std::io::Error::last_os_error().raw_os_error();
        }
    }
    DescriptorBudget {
        before,
        soft: finite(limit.rlim_cur),
        raised,
        os_error,
    }
}

#[cfg(not(unix))]
fn prepare_once() -> DescriptorBudget {
    DescriptorBudget {
        before: None,
        soft: None,
        raised: false,
        os_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_budget_caps_both_pools_without_overriding_smaller_explicit_limits() {
        let budget = DescriptorBudget {
            before: Some(256),
            soft: Some(256),
            raised: false,
            os_error: None,
        };
        let config = budget.admission(ConnectionAdmissionConfig::default());
        assert_eq!(config.requests.maximum, 24);
        assert_eq!(config.websockets.maximum, 24);
        assert_eq!(config.requests.initial, 24);
        assert_eq!(config.websockets.initial, 24);
        assert!(config.requests.maximum * 4 + config.websockets.maximum * 4 + 64 <= 256);
        let mut smaller = config;
        smaller.requests.initial = 2;
        smaller.requests.maximum = 4;
        smaller.websockets.initial = 1;
        smaller.websockets.maximum = 3;
        let config = budget.admission(smaller);
        assert_eq!(config.requests.maximum, 4);
        assert_eq!(config.requests.initial, 2);
        assert_eq!(config.websockets.maximum, 3);
        assert_eq!(config.websockets.initial, 1);
        let unlimited = DescriptorBudget {
            soft: None,
            ..budget
        }
        .admission(ConnectionAdmissionConfig::default());
        assert_eq!(unlimited.requests.maximum, 256);
        assert_eq!(unlimited.websockets.maximum, 224);
        let tiny = DescriptorBudget {
            soft: Some(32),
            ..budget
        }
        .admission(ConnectionAdmissionConfig::default());
        assert_eq!(tiny.requests.maximum, 1);
        assert_eq!(tiny.websockets.maximum, 1);
    }
}
