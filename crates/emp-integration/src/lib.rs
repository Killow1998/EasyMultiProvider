//! Transactional ownership of the top-level Codex TOML fields used by EMP.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

mod fields;
mod files;
mod manager;
mod storage;
#[cfg(test)]
mod tests;

pub mod runtime;
pub mod search;
pub(crate) use files::{MAX_CODEX_CONFIG_BYTES, MAX_LEASE_BYTES, absolute, read_text_limited};
pub use manager::IntegrationManager;

pub const REALTIME_SIDEBAND_FIELD: &str = "experimental_realtime_ws_base_url";
pub const MANAGED_FIELDS: [&str; 3] = [
    "openai_base_url",
    "model_catalog_json",
    REALTIME_SIDEBAND_FIELD,
];
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldState {
    pub present: bool,
    pub value: Option<String>,
}

impl FieldState {
    pub fn absent() -> Self {
        Self {
            present: false,
            value: None,
        }
    }
    pub fn value(value: impl Into<String>) -> Self {
        Self {
            present: true,
            value: Some(value.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldRecovery {
    pub original: FieldState,
    pub applied: FieldState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRecord {
    schema: String,
    version: u64,
    config_path: String,
    config_existed: bool,
    pub fields: BTreeMap<String, FieldRecovery>,
    lease_id: String,
    pub instance_id: String,
    pid: u32,
    pub status: String,
    created_at: String,
    updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationStatus {
    pub state: String,
    pub relation: String,
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub fields: BTreeMap<String, FieldState>,
    pub lease: Option<LeaseRecord>,
    pub same_instance: bool,
    pub conflicts: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationResult {
    pub action: String,
    pub state: String,
    pub relation: String,
    pub fields: BTreeMap<String, FieldState>,
    pub lease: Option<LeaseRecord>,
    pub conflicts: Vec<String>,
}

impl IntegrationResult {
    pub fn ok(&self) -> bool {
        self.state != "conflict" && self.conflicts.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationError(&'static str);

impl fmt::Display for IntegrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for IntegrationError {}
