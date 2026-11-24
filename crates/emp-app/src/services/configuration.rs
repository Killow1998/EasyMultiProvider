//! Readers cannot publish edits; serialize writes and publish only after durable transactions and reloads succeed.

use emp_state::{
    ConfigError, FileTransaction, VaultStore,
    config::{load_configuration_with_registry, save_configuration_in_transaction_with_registry},
    official_registry::RegistrySource,
    with_file_transaction,
};
use serde_json::Value;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub(crate) mod settings;

pub(crate) fn load_server_configuration(
    path: &Path,
    registry: RegistrySource,
) -> Result<Value, ConfigError> {
    let config = load_configuration_with_registry(Some(path), registry)?;
    #[cfg(test)]
    let config = {
        let mut config = config;
        let defaults = emp_state::normalize_configuration(None)?;
        if config["native_catalog_path"] == defaults["native_catalog_path"]
            || config["native_catalog_path"] == ""
        {
            config["native_catalog_path"] = serde_json::json!(fixture_native_catalog_path(path));
        }
        config
    };
    Ok(config)
}

#[cfg(test)]
pub(crate) fn fixture_native_catalog_path(config: &Path) -> PathBuf {
    config.with_file_name("codex").join("models_cache.json")
}

pub(crate) enum ChangeError {
    Invalid(String),
    Unavailable,
}

impl ChangeError {
    pub(super) fn invalid(error: impl std::fmt::Display) -> Self {
        Self::Invalid(error.to_string())
    }
}

pub(crate) struct ConfigurationState {
    config: Mutex<Value>,
    pub(crate) discovery_lock: Mutex<()>,
    pub(crate) config_path: PathBuf,
    pub(crate) vault: VaultStore,
    pub(crate) official_models: RegistrySource,
}

pub(crate) struct ConfigurationRead<'a>(MutexGuard<'a, Value>);

impl Deref for ConfigurationRead<'_> {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}

pub(super) struct ConfigurationEdit<'a> {
    current: ConfigurationRead<'a>,
    store: &'a ConfigurationState,
}

impl Deref for ConfigurationEdit<'_> {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.current
    }
}

impl ConfigurationState {
    pub(crate) fn new(
        config: Value,
        config_path: PathBuf,
        vault: VaultStore,
        official_models: RegistrySource,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            discovery_lock: Mutex::new(()),
            config_path,
            vault,
            official_models,
        }
    }

    pub(crate) fn read(&self) -> Result<ConfigurationRead<'_>, ()> {
        self.config.lock().map(ConfigurationRead).map_err(|_| ())
    }

    pub(crate) fn snapshot(&self) -> Result<Value, ()> {
        self.read().map(|config| config.clone())
    }

    /// CLI listener overrides describe this process; startup does not save them.
    pub(crate) fn set_listener(&self, host: std::net::IpAddr, port: u16) -> Result<(), ()> {
        let mut config = self.config.lock().map_err(|_| ())?;
        config["host"] = Value::String(host.to_string());
        config["port"] = Value::from(port);
        Ok(())
    }

    pub(super) fn edit(&self) -> Result<ConfigurationEdit<'_>, ()> {
        Ok(ConfigurationEdit {
            current: self.read()?,
            store: self,
        })
    }

    #[cfg(test)]
    pub(crate) fn test_config(&self) -> &Mutex<Value> {
        &self.config
    }
}

impl ConfigurationEdit<'_> {
    fn persist(
        &self,
        candidate: &Value,
        files: &mut FileTransaction,
    ) -> Result<Value, ConfigError> {
        let mut candidate = candidate.clone();
        emp_state::config::preserve_service_route_names(&self.current, &mut candidate);
        save_configuration_in_transaction_with_registry(
            &candidate,
            Some(&self.store.config_path),
            &self.store.vault,
            files,
            self.store.official_models,
        )?;
        load_server_configuration(&self.store.config_path, self.store.official_models)
    }

    pub(super) fn commit(&mut self, candidate: &Value) -> Result<(), ConfigError> {
        self.commit_with(candidate, |_, _| Ok(()))
    }

    /// Include derived files in the same transaction, using normalized saved
    /// configuration. Failed persistence or derived writes leave memory intact.
    pub(super) fn commit_with<T>(
        &mut self,
        candidate: &Value,
        derived: impl FnOnce(&Value, &mut FileTransaction) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        let (saved, result) = with_file_transaction(|files| {
            let saved = self.persist(candidate, files)?;
            let result = derived(&saved, files)?;
            Ok::<_, ConfigError>((saved, result))
        })?;
        *self.current.0 = saved;
        Ok(result)
    }

    /// Account operations may already have staged credential files while
    /// holding their refresh lock. Finish those files and config atomically.
    pub(super) fn commit_files(
        &mut self,
        candidate: &Value,
        mut files: FileTransaction,
    ) -> Result<(), ConfigError> {
        match self.persist(candidate, &mut files) {
            Ok(saved) => {
                files.commit();
                *self.current.0 = saved;
                Ok(())
            }
            Err(error) => {
                files.rollback()?;
                Err(error)
            }
        }
    }

    pub(super) fn import_migration(
        &mut self,
        decrypted: emp_state::migration::DecryptedMigration,
    ) -> Result<emp_state::migration::MigrationImportSummary, emp_state::migration::MigrationError>
    {
        let (saved, summary) = emp_state::migration::apply_migration_import_with_registry(
            &self.current,
            decrypted,
            &self.store.config_path,
            &self.store.vault,
            self.store.official_models,
        )?;
        *self.current.0 = saved;
        Ok(summary)
    }

    pub(super) fn commit_migration(
        &mut self,
        target: &Value,
        auth: &std::collections::BTreeMap<String, Value>,
    ) -> Result<(), emp_state::migration::MigrationError> {
        let saved = emp_state::migration::persist_migration_import_with_registry(
            target,
            auth,
            &self.store.config_path,
            &self.store.vault,
            self.store.official_models,
        )?;
        *self.current.0 = saved;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Language {
    English,
    SimplifiedChinese,
}

impl Language {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
        }
    }
}

/// A language change updates only its preference, without catalog or engine work.
pub(crate) fn update_language(
    state: &crate::app::ServerState,
    language: Language,
) -> Result<(), String> {
    let language = language.as_str();
    let mut edit = state
        .backend
        .configuration
        .edit()
        .map_err(|_| "internal server error")?;
    if edit["language"] == language {
        return Ok(());
    }
    let mut candidate = (*edit).clone();
    candidate["language"] = serde_json::Value::String(language.into());
    edit.commit(&candidate).map_err(|error| error.to_string())
}
