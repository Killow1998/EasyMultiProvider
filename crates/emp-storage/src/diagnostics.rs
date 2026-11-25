mod journal;
use emp_domain::MODEL_TEST_OPERATIONS;
use emp_files::{IntegrationFileLock, LockError};
pub use journal::Journal;
use std::path::{Path, PathBuf};
use std::time::Duration;
const PROBE_CAPACITY: usize = 64;

pub struct ServiceOwner {
    state_root: PathBuf,
    _lock: IntegrationFileLock,
}

impl ServiceOwner {
    pub fn acquire(state_root: &Path) -> Result<Self, LockError> {
        let lock = IntegrationFileLock::acquire(
            &state_root.join("service.lock"),
            Duration::ZERO,
            Duration::ZERO,
        )?;
        Ok(Self {
            state_root: state_root.to_owned(),
            _lock: lock,
        })
    }
}

pub fn is_probe(operation: &str) -> bool {
    matches!(
        operation,
        "model_metadata" | "model_discovery" | "catalog_refresh"
    ) || MODEL_TEST_OPERATIONS.contains(&operation)
}

pub fn timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.microsecond()
    )
}
pub fn random_id(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    if getrandom::getrandom(&mut value).is_err() {
        return String::new();
    }
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
