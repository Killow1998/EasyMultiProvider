use emp_test_support::{TestBudget, test_timeout};
use std::time::Duration;

pub fn emp_startup_budget() -> TestBudget {
    test_timeout(Duration::from_secs(30))
}
