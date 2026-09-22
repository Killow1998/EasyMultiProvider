//! Python-compatible durable runtime accounting and offline status projection.
use crate::{IntegrationError, now};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA: &str = "easy-multi-provider.runtime-recovery";
const STATES: &[&str] = &[
    "not_checked",
    "catalog_unverified",
    "reload_required",
    "stopping",
    "emp_loaded",
    "native_loaded",
    "stopped_waiting_for_start",
    "stop_failed",
    "verification_failed",
    "unsupported",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRecord {
    schema: String,
    version: u64,
    pub state: String,
    pub target: String,
    pub configuration_relation: String,
    pub expected_models: Vec<String>,
    pub verified: bool,
    pub detail: String,
    pub updated_at: String,
}

impl RuntimeRecord {
    fn valid(&self) -> bool {
        self.schema == SCHEMA
            && self.version == 1
            && STATES.contains(&self.state.as_str())
            && ["emp", "native"].contains(&self.target.as_str())
            && ["unleased", "original", "applied", "mixed", "other"]
                .contains(&self.configuration_relation.as_str())
            && self.expected_models.len() <= 2000
            && self
                .expected_models
                .iter()
                .all(|model| !model.is_empty() && model.chars().count() <= 512)
            && self.detail.chars().count() <= 1024
    }
}

pub struct RuntimeStore {
    path: PathBuf,
}

impl RuntimeStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<RuntimeRecord>, IntegrationError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(IntegrationError("runtime recovery record is unreadable")),
        };
        let raw: Value = serde_json::from_slice(&bytes)
            .map_err(|_| IntegrationError("runtime recovery record is unreadable"))?;
        let mut record: RuntimeRecord = serde_json::from_value(raw)
            .map_err(|_| IntegrationError("runtime recovery record is invalid"))?;
        if !record.valid() {
            return Err(IntegrationError("runtime recovery record is invalid"));
        }
        let mut seen = BTreeSet::new();
        record
            .expected_models
            .retain(|model| seen.insert(model.clone()));
        Ok(Some(record))
    }

    pub fn save(
        &self,
        state: &str,
        target: &str,
        relation: &str,
        expected_models: &[String],
        verified: bool,
        detail: &str,
    ) -> Result<RuntimeRecord, IntegrationError> {
        let mut seen = BTreeSet::new();
        let models = expected_models
            .iter()
            .filter(|model| !model.is_empty() && model.chars().count() <= 512)
            .filter(|model| seen.insert((*model).clone()))
            .cloned()
            .collect::<Vec<_>>();
        if models.len() > 2000 {
            return Err(IntegrationError(
                "too many expected models for runtime recovery",
            ));
        }
        let record = RuntimeRecord {
            schema: SCHEMA.to_owned(),
            version: 1,
            state: state.to_owned(),
            target: target.to_owned(),
            configuration_relation: relation.to_owned(),
            expected_models: models,
            verified,
            detail: detail.chars().take(1024).collect(),
            updated_at: now(),
        };
        if !record.valid() {
            return Err(IntegrationError("runtime recovery state is invalid"));
        }
        let mut bytes = serde_json::to_vec_pretty(&record)
            .map_err(|_| IntegrationError("runtime recovery record is invalid"))?;
        bytes.push(b'\n');
        emp_state::filesystem::atomic_write_private_state(&self.path, &bytes)
            .map_err(|_| IntegrationError("unable to write runtime recovery state"))?;
        Ok(record)
    }
}

pub fn offline_snapshot(record: Option<&RuntimeRecord>, confidence: &str) -> Value {
    let Some(record) = record else {
        return json!({
            "state":"not_checked", "target":"native", "verified":false,
            "confidence":confidence, "detail":"Codex runtime has not been checked", "last_known":null
        });
    };
    let pending = matches!(record.state.as_str(), "reload_required" | "stopping");
    let detail = match record.state.as_str() {
        "reload_required" => "A previously requested runtime reload still requires confirmation",
        "stopping" => "A previous runtime operation was interrupted and must be retried",
        _ => "Last-known runtime status is stale; no live check was performed",
    };
    json!({
        "state":if pending {"reload_required"} else {"not_checked"},
        "target":record.target, "verified":false, "confidence":confidence, "detail":detail,
        "last_known":{"state":record.state,"target":record.target,"verified":record.verified,"observed_at":record.updated_at}
    })
}
