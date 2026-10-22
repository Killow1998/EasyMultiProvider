//! Compare-and-restore ownership of optional Codex preferences.
use crate::{
    IntegrationError, MAX_CODEX_CONFIG_BYTES, MAX_LEASE_BYTES, absolute, read_text_limited,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item};

const SCHEMA: &str = "easy-multi-provider.codex-preference";
const KEY: &str = "async_compaction";
const MEMORY_KEYS: [&str; 2] = ["extract_model", "consolidation_model"];

pub struct CodexPreferences {
    config: PathBuf,
    lease: PathBuf,
    memory: bool,
}

impl CodexPreferences {
    pub fn new(config: PathBuf, lease: PathBuf) -> Self {
        Self {
            config,
            lease,
            memory: false,
        }
    }
    fn memories(&self) -> Self {
        Self {
            config: self.config.clone(),
            lease: self.lease.with_file_name("memory-model.json"),
            memory: true,
        }
    }
    fn read(&self) -> Result<DocumentMut, IntegrationError> {
        reject_link(&self.config)?;
        match read_text_limited(&self.config, MAX_CODEX_CONFIG_BYTES) {
            Ok(raw) => raw
                .parse()
                .map_err(|_| IntegrationError("unable to parse Codex configuration")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
            Err(_) => Err(IntegrationError("unable to read Codex configuration")),
        }
    }
    fn read_lease(&self) -> Result<Option<Value>, IntegrationError> {
        reject_link(&self.lease)?;
        let raw = match read_text_limited(&self.lease, MAX_LEASE_BYTES) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(IntegrationError("unable to read Codex preference lease")),
        };
        let value: Value = serde_json::from_str(&raw)
            .map_err(|_| IntegrationError("invalid Codex preference lease"))?;
        if value["schema"] != SCHEMA
            || value["version"] != 1
            || value["memory"] != self.memory
            || value["config_path"] != absolute(&self.config)?.to_string_lossy().as_ref()
            || !matches!(
                value["status"].as_str(),
                Some("active" | "prepared" | "restored")
            )
            || !value["parent_present"].is_boolean()
        {
            return Err(IntegrationError("invalid Codex preference lease"));
        }
        for key in ["original", "applied", "previous"] {
            if !value[key].is_null() && !value[key].is_string() {
                return Err(IntegrationError("invalid Codex preference lease value"));
            }
            decode(&value[key])?;
        }
        Ok(Some(value))
    }
    fn save_lease(&self, value: &Value) -> Result<(), IntegrationError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| IntegrationError("unable to encode Codex preference lease"))?;
        emp_state::atomic_write_private_state(&self.lease, &bytes)
            .map_err(|_| IntegrationError("unable to write Codex preference lease"))
    }
    fn save(&self, document: &DocumentMut) -> Result<(), IntegrationError> {
        emp_state::filesystem::atomic_write_config(&self.config, document.to_string().as_bytes())
            .map_err(|_| IntegrationError("unable to update Codex preferences"))
    }
    /// The application holds its integration operation lock across preference writes.
    pub fn apply(&self, enabled: Option<bool>, memory_model: &str) -> Result<(), IntegrationError> {
        self.apply_background(enabled)?;
        self.apply_memory(memory_model)
    }
    pub fn rollback(
        &self,
        enabled: Option<bool>,
        memory_model: &str,
    ) -> Result<(), IntegrationError> {
        // Independent journals must both be attempted, including during rollback.
        let background = self.apply_background(enabled);
        let memory = self.apply_memory(memory_model);
        background.and(memory)
    }
    fn apply_background(&self, enabled: Option<bool>) -> Result<(), IntegrationError> {
        let Some(enabled) = enabled else {
            return self.restore_current();
        };
        let document = self.read()?;
        let mut value = feature(&document)?
            .cloned()
            .unwrap_or_else(|| toml_edit::value(enabled));
        if value.is_table_like() {
            value["enabled"] = toml_edit::value(enabled);
        } else {
            let mut replacement = toml_edit::Value::from(enabled);
            if let Some(current) = value.as_value() {
                *replacement.decor_mut() = current.decor().clone();
            }
            value = Item::Value(replacement);
        }
        self.apply_value(value)
    }
    /// Configuring models changes their client-side input budget, not memory permissions.
    fn apply_memory(&self, model: &str) -> Result<(), IntegrationError> {
        let manager = self.memories();
        if model.is_empty() {
            return manager.restore_current();
        }
        let mut table = toml_edit::Table::new();
        for key in MEMORY_KEYS {
            table.insert(key, toml_edit::value(model));
        }
        manager.apply_value(Item::Table(table))
    }
    fn apply_value(&self, value: Item) -> Result<(), IntegrationError> {
        let mut document = self.read()?;
        let current = encode(&document, self.memory)?;
        let previous = self
            .read_lease()?
            .filter(|lease| lease["status"] != "restored");
        if let Some(lease) = &previous
            && !matches_lease(&current, lease)?
        {
            return Err(IntegrationError(
                "Codex preferences were changed outside EMP",
            ));
        }
        let original = previous
            .as_ref()
            .map_or(current.clone(), |lease| lease["original"].clone());
        let parent_present = previous.as_ref().map_or(
            document
                .get(if self.memory { "memories" } else { "features" })
                .is_some(),
            |lease| lease["parent_present"] == true,
        );
        set_value(&mut document, Some(value), self.memory)?;
        let applied = encode(&document, self.memory)?;
        let mut lease = json!({"schema":SCHEMA,"version":1,"config_path":absolute(&self.config)?.to_string_lossy(),
            "memory":self.memory,"status":"prepared","original":original,"previous":current,
            "applied":applied,"parent_present":parent_present});
        self.save_lease(&lease)?;
        self.save(&document)?;
        if !same(&encode(&self.read()?, self.memory)?, &applied)? {
            return Err(IntegrationError("unable to verify Codex preferences"));
        }
        lease["status"] = json!("active");
        self.save_lease(&lease)
    }
    fn restore_current(&self) -> Result<(), IntegrationError> {
        let Some(mut lease) = self
            .read_lease()?
            .filter(|lease| lease["status"] != "restored")
        else {
            return Ok(());
        };
        let mut document = self.read()?;
        let current = encode(&document, self.memory)?;
        if !same(&current, &lease["original"])? {
            if !matches_lease(&current, &lease)? {
                return Err(IntegrationError(
                    "Codex preferences were changed outside EMP",
                ));
            }
            set_value(&mut document, decode(&lease["original"])?, self.memory)?;
            let parent = if self.memory { "memories" } else { "features" };
            if lease["parent_present"] == false
                && document
                    .get(parent)
                    .and_then(Item::as_table_like)
                    .is_some_and(|table| table.is_empty())
            {
                document.remove(parent);
            }
            self.save(&document)?;
            if !same(&encode(&self.read()?, self.memory)?, &lease["original"])? {
                return Err(IntegrationError("unable to restore Codex preferences"));
            }
        }
        lease["status"] = json!("restored");
        self.save_lease(&lease)
    }
    pub fn restore(&self) -> Result<(), IntegrationError> {
        // An external edit to one optional group must not skip the other restore.
        let background = self.restore_current();
        let memory = self.memories().restore_current();
        background.and(memory)
    }
    /// Credential-free hints; command-line/profile/session overrides remain client-owned.
    pub fn snapshot(&self) -> Result<Value, IntegrationError> {
        let document = self.read()?;
        let setting = feature(&document)?;
        let enabled = setting
            .and_then(|item| {
                item.as_bool()
                    .or_else(|| item.get("enabled").and_then(Item::as_bool))
            })
            .unwrap_or(false);
        let threshold = setting
            .and_then(|item| item.get("model_async_compact_threshold_percent"))
            .and_then(Item::as_integer)
            .unwrap_or(65);
        Ok(
            json!({"background_enabled":enabled,"background_threshold_percent":threshold,
            "model_context_window":document.get("model_context_window").and_then(Item::as_integer),
            "model_auto_compact_token_limit":document.get("model_auto_compact_token_limit").and_then(Item::as_integer),
            "model_auto_compact_token_limit_scope":document.get("model_auto_compact_token_limit_scope").and_then(Item::as_str).unwrap_or("total"),
            "source":"user_config","restart_required":true}),
        )
    }
}

fn reject_link(path: &Path) -> Result<(), IntegrationError> {
    if path.is_symlink() {
        Err(IntegrationError(
            "Codex preference path must not be a symlink",
        ))
    } else {
        Ok(())
    }
}
fn matches_lease(current: &Value, lease: &Value) -> Result<bool, IntegrationError> {
    Ok(same(current, &lease["original"])?
        || same(current, &lease["applied"])?
        || (lease["status"] == "prepared" && same(current, &lease["previous"])?))
}

mod values;
use values::{decode, encode, feature, same, set_value};
#[cfg(test)]
mod tests;
