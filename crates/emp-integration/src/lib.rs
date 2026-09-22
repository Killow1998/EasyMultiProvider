//! Transactional ownership of the two top-level Codex TOML fields used by EMP.

use emp_state::IntegrationFileLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub mod runtime;

pub const MANAGED_FIELDS: [&str; 2] = ["openai_base_url", "model_catalog_json"];
const LEASE_SCHEMA: &str = "easy-multi-provider.integration-lease";
const LEASE_VERSION: u64 = 2;

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

pub struct IntegrationManager {
    config_path: PathBuf,
    lease_path: PathBuf,
    lock_path: PathBuf,
    pub instance_id: String,
    lock_timeout: Duration,
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
        let lease = self.read_lease()?;
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
        if !service_ready {
            return Err(IntegrationError(
                "EMP service must be listening and accepting requests",
            ));
        }
        validate_value(base_url)?;
        if let Some(catalog) = catalog {
            validate_value(catalog)?;
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
        ]);
        if let Some(mut lease) = self.read_lease()?
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
        let Some(mut lease) = self.read_lease()? else {
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

    fn lock(&self) -> Result<IntegrationFileLock, IntegrationError> {
        IntegrationFileLock::acquire(
            &self.lock_path,
            self.lock_timeout,
            Duration::from_millis(20),
        )
        .map_err(|_| IntegrationError("unable to acquire integration lock"))
    }

    fn assert_safe_paths(&self) -> Result<(), IntegrationError> {
        for path in [&self.config_path, &self.lease_path, &self.lock_path] {
            if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(IntegrationError("integration path must not be a symlink"));
            }
        }
        Ok(())
    }

    fn read_config(&self) -> Result<(String, bool), IntegrationError> {
        match fs::read_to_string(&self.config_path) {
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

    fn read_lease(&self) -> Result<Option<LeaseRecord>, IntegrationError> {
        let raw = match fs::read(&self.lease_path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(IntegrationError("unable to read integration lease")),
        };
        let lease: LeaseRecord = serde_json::from_slice(&raw)
            .map_err(|_| IntegrationError("unable to read integration lease"))?;
        if lease.schema != LEASE_SCHEMA
            || lease.version != LEASE_VERSION
            || lease.config_path != absolute(&self.config_path)?.to_string_lossy()
            || !matches!(
                lease.status.as_str(),
                "prepared" | "active" | "restoring" | "restored"
            )
            || lease.fields.len() != 2
            || MANAGED_FIELDS
                .iter()
                .any(|name| !lease.fields.contains_key(*name))
        {
            return Err(IntegrationError("unsupported integration lease"));
        }
        Ok(Some(lease))
    }

    fn write_lease(&self, lease: &LeaseRecord) -> Result<(), IntegrationError> {
        let mut bytes = serde_json::to_vec_pretty(lease)
            .map_err(|_| IntegrationError("unable to write integration lease"))?;
        bytes.push(b'\n');
        emp_state::filesystem::atomic_write_private_state(&self.lease_path, &bytes)
            .map_err(|_| IntegrationError("unable to write integration lease"))
    }

    fn make_lease(
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

    fn transition(
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

fn states(document: &str) -> Result<BTreeMap<String, FieldState>, IntegrationError> {
    let parsed = document
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| IntegrationError("unable to parse Codex TOML config"))?;
    MANAGED_FIELDS
        .into_iter()
        .map(|name| {
            let state = match parsed.get(name) {
                None => FieldState::absent(),
                Some(value) => FieldState::value(
                    value
                        .as_str()
                        .ok_or(IntegrationError("managed TOML field is not a string"))?,
                ),
            };
            Ok((name.to_owned(), state))
        })
        .collect()
}

fn set_states(
    document: String,
    desired: &BTreeMap<String, FieldState>,
) -> Result<String, IntegrationError> {
    // Root comments are independent of the first key/table in tomlkit. Keep
    // them outside the edit tree so removing a managed key cannot remove them.
    let header_len: usize = document
        .split_inclusive('\n')
        .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
        .map(str::len)
        .sum();
    let (header, body) = document.split_at(header_len);
    let mut parsed = body
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| IntegrationError("unable to parse Codex TOML config"))?;
    let mut inserted = false;
    for name in MANAGED_FIELDS {
        let state = &desired[name];
        if !state.present {
            parsed.as_table_mut().remove(name);
            continue;
        }
        let mut replacement = toml_edit::Value::from(state.value.as_deref().unwrap_or_default());
        if let Some(current) = parsed.get(name).and_then(toml_edit::Item::as_value) {
            *replacement.decor_mut() = current.decor().clone();
        } else {
            inserted = true;
        }
        parsed[name] = toml_edit::Item::Value(replacement);
    }
    if inserted {
        // Python's tomlkit retains this separator when managed fields are removed.
        for (_, item) in parsed.iter_mut() {
            let table = match item {
                toml_edit::Item::Table(table) => Some(table),
                toml_edit::Item::ArrayOfTables(tables) => tables.iter_mut().next(),
                _ => None,
            };
            if let Some(table) = table {
                let prefix = table
                    .decor()
                    .prefix()
                    .and_then(|raw| raw.as_str())
                    .unwrap_or("");
                if !prefix.starts_with(['\r', '\n']) {
                    let prefix = format!("\n{prefix}");
                    table.decor_mut().set_prefix(prefix);
                }
                break;
            }
        }
    }
    let rendered = parsed.to_string();
    let newline = if !header.is_empty() && !header.ends_with('\n') && !rendered.is_empty() {
        "\n"
    } else {
        ""
    };
    Ok(format!("{header}{newline}{rendered}"))
}

fn relation(current: &BTreeMap<String, FieldState>, lease: &LeaseRecord) -> String {
    let original = lease
        .fields
        .iter()
        .map(|(name, recovery)| (name.clone(), recovery.original.clone()))
        .collect::<BTreeMap<_, _>>();
    let applied = lease
        .fields
        .iter()
        .map(|(name, recovery)| (name.clone(), recovery.applied.clone()))
        .collect::<BTreeMap<_, _>>();
    if current == &original {
        "original"
    } else if current == &applied {
        "applied"
    } else if MANAGED_FIELDS
        .iter()
        .all(|name| current[*name] == original[*name] || current[*name] == applied[*name])
    {
        "mixed"
    } else {
        "other"
    }
    .to_owned()
}

fn conflict_names(current: &BTreeMap<String, FieldState>, lease: &LeaseRecord) -> Vec<String> {
    let mut values = MANAGED_FIELDS
        .iter()
        .filter(|name| {
            current[**name] != lease.fields[**name].original
                && current[**name] != lease.fields[**name].applied
        })
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();
    if values.is_empty() && relation(current, lease) == "mixed" {
        values.push("mixed_state".to_owned());
    }
    values
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

fn validate_value(value: &str) -> Result<(), IntegrationError> {
    if value.contains(['\n', '\r']) {
        Err(IntegrationError(
            "managed value must be a single-line string",
        ))
    } else {
        Ok(())
    }
}
fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}
fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0; bytes];
    let _ = getrandom::getrandom(&mut raw);
    raw.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn absolute(path: &Path) -> Result<PathBuf, IntegrationError> {
    std::path::absolute(path).map_err(|_| IntegrationError("integration path is invalid"))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), IntegrationError> {
    emp_state::filesystem::atomic_write_config(path, bytes)
        .map_err(|_| IntegrationError("unable to write integration state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_preserves_unmanaged_toml_style_and_restore_is_exact_for_fields() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let lease = directory.path().join("state/lease.json");
        fs::write(&config,"# keep\nopenai_base_url   = \"native\"  # inline\ntitle = \"keep\"\n[nested]\nopenai_base_url = \"nested\"\n").unwrap();
        let manager =
            IntegrationManager::new(&config, &lease, Some("instance".to_owned())).unwrap();
        assert_eq!(
            manager
                .enable("http://127.0.0.1:123/v1", Some("catalog.json"), true)
                .unwrap()
                .state,
            "active"
        );
        let applied = fs::read_to_string(&config).unwrap();
        assert!(applied.contains("openai_base_url   = \"http://127.0.0.1:123/v1\"  # inline"));
        assert!(applied.contains("[nested]\nopenai_base_url = \"nested\""));
        manager.restore().unwrap();
        let restored = fs::read_to_string(&config).unwrap();
        assert!(restored.contains("openai_base_url   = \"native\"  # inline"));
        assert!(restored.contains("title = \"keep\""));
    }
}
