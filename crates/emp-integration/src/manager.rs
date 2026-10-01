//! Integration operations: status, enable, restore and recovery under the lease lock.
use crate::fields::{conflict_names, relation, set_states, states, validate_value};
use crate::files::{absolute, atomic_write};
use crate::storage::random_hex;
use crate::{
    FieldState, IntegrationError, IntegrationResult, IntegrationStatus, LeaseRecord,
    REALTIME_SIDEBAND_FIELD,
};
use emp_state::IntegrationFileLock;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct IntegrationManager {
    pub(super) config_path: PathBuf,
    pub(super) lease_path: PathBuf,
    pub(super) lock_path: PathBuf,
    pub instance_id: String,
    pub(super) lock_timeout: Duration,
}

impl IntegrationManager {
    pub fn new(
        config_path: impl Into<PathBuf>,
        lease_path: impl Into<PathBuf>,
        instance_id: Option<String>,
    ) -> Result<Self, IntegrationError> {
        let config_path = config_path.into();
        let lease_path = lease_path.into();
        if absolute(&config_path)? == absolute(&lease_path)? {
            return Err(IntegrationError("config and lease paths must differ"));
        }
        Ok(Self {
            config_path,
            lock_path: PathBuf::from(format!("{}.lock", lease_path.display())),
            lease_path,
            instance_id: instance_id.unwrap_or_else(|| format!("instance-{}", random_hex(16))),
            lock_timeout: Duration::from_secs(5),
        })
    }

    pub fn with_lock_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.lock_path = path.into();
        self
    }

    pub fn operation_lock(&self) -> Result<IntegrationFileLock, IntegrationError> {
        IntegrationFileLock::acquire(
            &self.lease_path.with_file_name("operation.lock"),
            self.lock_timeout,
            Duration::from_millis(20),
        )
        .map_err(|_| IntegrationError("unable to acquire integration operation lock"))
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn lease_path(&self) -> &Path {
        &self.lease_path
    }

    pub fn status(&self) -> Result<IntegrationStatus, IntegrationError> {
        self.assert_safe_paths()?;
        let _lock = self.lock()?;
        let (document, exists) = self.read_config()?;
        let fields = states(&document)?;
        let lease = self.read_lease(&fields)?;
        let Some(lease) = lease else {
            return Ok(IntegrationStatus {
                state: "native".to_owned(),
                relation: "unleased".to_owned(),
                config_path: self.config_path.clone(),
                config_exists: exists,
                fields,
                lease: None,
                same_instance: false,
                conflicts: Vec::new(),
            });
        };
        let relation = relation(&fields, &lease);
        let mut conflicts = Vec::new();
        if relation == "other" || lease.status == "restored" && relation != "original" {
            conflicts.push("lease_state_mismatch".to_owned());
            conflicts.extend(conflict_names(&fields, &lease));
            conflicts.sort();
            conflicts.dedup();
        }
        Ok(IntegrationStatus {
            state: if conflicts.is_empty() {
                lease.status.clone()
            } else {
                "conflict".to_owned()
            },
            relation,
            config_path: self.config_path.clone(),
            config_exists: exists,
            same_instance: lease.instance_id == self.instance_id,
            fields,
            lease: Some(lease),
            conflicts,
        })
    }

    pub fn enable(
        &self,
        base_url: &str,
        catalog: Option<&str>,
        service_ready: bool,
    ) -> Result<IntegrationResult, IntegrationError> {
        self.enable_with_sideband(base_url, catalog, service_ready, None)
    }

    pub fn enable_with_sideband(
        &self,
        base_url: &str,
        catalog: Option<&str>,
        service_ready: bool,
        realtime_sideband_base_url: Option<&str>,
    ) -> Result<IntegrationResult, IntegrationError> {
        if !service_ready {
            return Err(IntegrationError(
                "EMP service must be listening and accepting requests",
            ));
        }
        validate_value(base_url)?;
        if let Some(catalog) = catalog {
            validate_value(catalog)?;
        }
        if let Some(sideband_url) = realtime_sideband_base_url {
            validate_value(sideband_url)?;
        }
        self.assert_safe_paths()?;
        let _lock = self.lock()?;
        let (mut document, existed) = self.read_config()?;
        let current = states(&document)?;
        let desired = BTreeMap::from([
            ("openai_base_url".to_owned(), FieldState::value(base_url)),
            (
                "model_catalog_json".to_owned(),
                catalog.map_or_else(FieldState::absent, FieldState::value),
            ),
            (
                REALTIME_SIDEBAND_FIELD.to_owned(),
                realtime_sideband_base_url.map_or_else(
                    || current[REALTIME_SIDEBAND_FIELD].clone(),
                    FieldState::value,
                ),
            ),
        ]);
        if let Some(mut lease) = self.read_lease(&current)?
            && lease.status != "restored"
        {
            let relation = relation(&current, &lease);
            if matches!(relation.as_str(), "other" | "mixed") {
                return Ok(result(
                    "conflict",
                    "conflict",
                    &relation,
                    current,
                    Some(lease.clone()),
                    conflict_names(&states(&document)?, &lease),
                ));
            }
            if relation == "applied" {
                let applied = lease
                    .fields
                    .iter()
                    .map(|(name, recovery)| (name.clone(), recovery.applied.clone()))
                    .collect::<BTreeMap<_, _>>();
                if desired != applied {
                    return Ok(result(
                        "conflict",
                        "conflict",
                        &relation,
                        current,
                        Some(lease),
                        vec!["active_lease".to_owned()],
                    ));
                }
                lease = self.transition(lease, "active", true)?;
                self.write_lease(&lease)?;
                return Ok(result(
                    "re_adopted",
                    "active",
                    "applied",
                    desired,
                    Some(lease),
                    Vec::new(),
                ));
            }
            lease = self.transition(lease, "restored", false)?;
            self.write_lease(&lease)?;
        }
        let mut lease = self.make_lease(&current, &desired, existed, "prepared")?;
        self.write_lease(&lease)?;
        document = set_states(document, &desired)?;
        atomic_write(&self.config_path, document.as_bytes())?;
        let (verified, _) = self.read_config()?;
        let verified = states(&verified)?;
        if verified != desired {
            return Ok(result(
                "conflict",
                "conflict",
                &relation(&verified, &lease),
                verified.clone(),
                Some(lease.clone()),
                conflict_names(&verified, &lease),
            ));
        }
        lease = self.transition(lease, "active", false)?;
        self.write_lease(&lease)?;
        Ok(result(
            "enabled",
            "active",
            "applied",
            desired,
            Some(lease),
            Vec::new(),
        ))
    }

    pub fn restore(&self) -> Result<IntegrationResult, IntegrationError> {
        self.assert_safe_paths()?;
        let _lock = self.lock()?;
        let (mut document, exists) = self.read_config()?;
        let current = states(&document)?;
        let Some(mut lease) = self.read_lease(&current)? else {
            return Ok(result(
                "noop",
                "native",
                "unleased",
                current,
                None,
                Vec::new(),
            ));
        };
        let current_relation = relation(&current, &lease);
        if lease.status == "restored" {
            return if current_relation == "original" {
                Ok(result(
                    "noop",
                    "restored",
                    &current_relation,
                    current,
                    Some(lease),
                    Vec::new(),
                ))
            } else {
                let conflicts = conflict_names(&current, &lease);
                Ok(result(
                    "conflict",
                    "conflict",
                    &current_relation,
                    current,
                    Some(lease),
                    conflicts,
                ))
            };
        }
        if current_relation == "other" {
            let conflicts = conflict_names(&current, &lease);
            return Ok(result(
                "conflict",
                "conflict",
                &current_relation,
                current,
                Some(lease),
                conflicts,
            ));
        }
        lease = self.transition(lease, "restoring", false)?;
        self.write_lease(&lease)?;
        let original = lease
            .fields
            .iter()
            .map(|(name, recovery)| (name.clone(), recovery.original.clone()))
            .collect::<BTreeMap<_, _>>();
        document = set_states(document, &original)?;
        if !lease.config_existed && document.trim().is_empty() {
            match fs::remove_file(&self.config_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(IntegrationError("unable to restore Codex TOML config")),
            }
        } else if exists || !document.is_empty() {
            atomic_write(&self.config_path, document.as_bytes())?;
        }
        let verified = self.read_config()?.0;
        let verified = states(&verified)?;
        if verified != original {
            let conflicts = conflict_names(&verified, &lease);
            return Ok(result(
                "conflict",
                "conflict",
                &relation(&verified, &lease),
                verified,
                Some(lease),
                conflicts,
            ));
        }
        lease = self.transition(lease, "restored", false)?;
        self.write_lease(&lease)?;
        Ok(result(
            "restored",
            "restored",
            "original",
            original,
            Some(lease),
            Vec::new(),
        ))
    }

    pub fn recover(
        &self,
        re_adopt: bool,
        service_ready: bool,
    ) -> Result<IntegrationResult, IntegrationError> {
        if !re_adopt {
            return self.restore();
        }
        if !service_ready {
            return Err(IntegrationError(
                "EMP service must be listening and accepting requests",
            ));
        }
        let status = self.status()?;
        let Some(lease) = status.lease else {
            return Ok(result(
                "noop",
                "native",
                "unleased",
                status.fields,
                None,
                Vec::new(),
            ));
        };
        if status.state == "conflict" || status.relation == "mixed" {
            return Ok(result(
                "conflict",
                "conflict",
                &status.relation,
                status.fields,
                Some(lease),
                status.conflicts,
            ));
        }
        if status.relation == "original" {
            return self.restore();
        }
        let _lock = self.lock()?;
        let adopted = self.transition(lease, "active", true)?;
        self.write_lease(&adopted)?;
        Ok(result(
            "re_adopted",
            "active",
            "applied",
            status.fields,
            Some(adopted),
            Vec::new(),
        ))
    }
}

fn result(
    action: &str,
    state: &str,
    relation: &str,
    fields: BTreeMap<String, FieldState>,
    lease: Option<LeaseRecord>,
    conflicts: Vec<String>,
) -> IntegrationResult {
    IntegrationResult {
        action: action.to_owned(),
        state: state.to_owned(),
        relation: relation.to_owned(),
        fields,
        lease,
        conflicts,
    }
}
