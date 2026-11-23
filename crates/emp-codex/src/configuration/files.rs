use super::IntegrationError;
use std::path::{Path, PathBuf};

pub(crate) fn absolute(path: &Path) -> Result<PathBuf, IntegrationError> {
    std::path::absolute(path).map_err(|_| IntegrationError("integration path is invalid"))
}

pub(crate) const MAX_CODEX_CONFIG_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_LEASE_BYTES: usize = 1024 * 1024;

pub(crate) fn read_text_limited(path: &Path, limit: usize) -> std::io::Result<String> {
    String::from_utf8(emp_state::read_file_limited(path, limit)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), IntegrationError> {
    emp_state::filesystem::atomic_write_config(path, bytes)
        .map_err(|_| IntegrationError("unable to write integration state"))
}
