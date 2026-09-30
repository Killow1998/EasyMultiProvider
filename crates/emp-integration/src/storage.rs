//! Lease validation, durable writes and path/lock checks; no integration dispatch.
use crate::fields::states;
use crate::files::{MAX_CODEX_CONFIG_BYTES, MAX_LEASE_BYTES, absolute, read_text_limited};
use crate::{
    FieldRecovery, FieldState, IntegrationError, IntegrationManager, LeaseRecord, MANAGED_FIELDS,
    REALTIME_SIDEBAND_FIELD,
};
use emp_state::IntegrationFileLock;
use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const LEGACY_MANAGED_FIELDS: [&str; 2] = ["openai_base_url", "model_catalog_json"];
const LEASE_SCHEMA: &str = "easy-multi-provider.integration-lease";
const LEGACY_LEASE_VERSION: u64 = 2;
const LEASE_VERSION: u64 = 3;

impl IntegrationManager {
    pub(super) fn lock(&self) -> Result<IntegrationFileLock, IntegrationError> {
        IntegrationFileLock::acquire(
            &self.lock_path,
            self.lock_timeout,
            Duration::from_millis(20),
        )
        .map_err(|_| IntegrationError("unable to acquire integration lock"))
    }

    pub(super) fn assert_safe_paths(&self) -> Result<(), IntegrationError> {
        for path in [&self.config_path, &self.lease_path, &self.lock_path] {
            if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(IntegrationError("integration path must not be a symlink"));
            }
        }
        Ok(())
    }

    pub(super) fn read_config(&self) -> Result<(String, bool), IntegrationError> {
        match read_text_limited(&self.config_path, MAX_CODEX_CONFIG_BYTES) {
            Ok(value) => {
                states(&value)?;
                Ok((value, true))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((String::new(), false))
            }
            Err(_) => Err(IntegrationError("unable to parse Codex TOML config")),
        }
    }

    pub(super) fn read_lease(
        &self,
        current: &BTreeMap<String, FieldState>,
    ) -> Result<Option<LeaseRecord>, IntegrationError> {
        let raw = match emp_state::read_file_limited(&self.lease_path, MAX_LEASE_BYTES) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(IntegrationError("unable to read integration lease")),
        };
        let mut lease: LeaseRecord = serde_json::from_slice(&raw)
            .map_err(|_| IntegrationError("unable to read integration lease"))?;
        let lease_fields: &[&str] = if lease.version == LEGACY_LEASE_VERSION {
            &LEGACY_MANAGED_FIELDS
        } else {
            &MANAGED_FIELDS
        };
        let known_shape = lease.schema == LEASE_SCHEMA
            && matches!(lease.version, LEGACY_LEASE_VERSION | LEASE_VERSION)
            && lease.config_path == absolute(&self.config_path)?.to_string_lossy()
            && matches!(
                lease.status.as_str(),
                "prepared" | "active" | "restoring" | "restored"
            );
        let managed_fields_complete = lease_fields
            .iter()
            .all(|name| lease.fields.contains_key(*name));
        if !known_shape || lease.fields.len() != lease_fields.len() || !managed_fields_complete {
            return Err(IntegrationError("unsupported integration lease"));
        }
        if lease.version == LEGACY_LEASE_VERSION {
            let sideband = current[REALTIME_SIDEBAND_FIELD].clone();
            lease.fields.insert(
                REALTIME_SIDEBAND_FIELD.to_owned(),
                FieldRecovery {
                    original: sideband.clone(),
                    applied: sideband,
                },
            );
            lease.version = LEASE_VERSION;
        }
        Ok(Some(lease))
    }

    pub(super) fn write_lease(&self, lease: &LeaseRecord) -> Result<(), IntegrationError> {
        let mut bytes = serde_json::to_vec_pretty(lease)
            .map_err(|_| IntegrationError("unable to write integration lease"))?;
        bytes.push(b'\n');
        emp_state::filesystem::atomic_write_private_state(&self.lease_path, &bytes)
            .map_err(|_| IntegrationError("unable to write integration lease"))
    }

    pub(super) fn make_lease(
        &self,
        original: &BTreeMap<String, FieldState>,
        applied: &BTreeMap<String, FieldState>,
        existed: bool,
        status: &str,
    ) -> Result<LeaseRecord, IntegrationError> {
        let now = now();
        Ok(LeaseRecord {
            schema: LEASE_SCHEMA.to_owned(),
            version: LEASE_VERSION,
            config_path: absolute(&self.config_path)?.to_string_lossy().into_owned(),
            config_existed: existed,
            fields: MANAGED_FIELDS
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        FieldRecovery {
                            original: original[name].clone(),
                            applied: applied[name].clone(),
                        },
                    )
                })
                .collect(),
            lease_id: format!("lease-{}", random_hex(16)),
            instance_id: self.instance_id.clone(),
            pid: std::process::id(),
            status: status.to_owned(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub(super) fn transition(
        &self,
        mut lease: LeaseRecord,
        status: &str,
        re_adopt: bool,
    ) -> Result<LeaseRecord, IntegrationError> {
        let allowed = match lease.status.as_str() {
            "prepared" | "active" | "restoring" => {
                matches!(status, "active" | "restoring" | "restored")
            }
            "restored" => status == "restored",
            _ => false,
        };
        if !allowed {
            return Err(IntegrationError("invalid lease transition"));
        }
        if re_adopt {
            lease.lease_id = format!("lease-{}", random_hex(16));
            lease.instance_id = self.instance_id.clone();
            lease.pid = std::process::id();
        }
        lease.status = status.to_owned();
        lease.updated_at = now();
        Ok(lease)
    }
}

pub(super) fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
pub(super) fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0; bytes];
    let _ = getrandom::getrandom(&mut raw);
    raw.iter().map(|byte| format!("{byte:02x}")).collect()
}
