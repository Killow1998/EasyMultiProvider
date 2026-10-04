//! Serialized configuration ownership. Readers cannot publish in-memory edits;
//! writers publish only after their durable transaction and reload succeed.

use emp_state::{
    ConfigError, FileTransaction, VaultStore, load_configuration,
    save_configuration_in_transaction, with_file_transaction,
};
use serde_json::Value;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

pub(crate) mod settings;

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
    pub(crate) fn new(config: Value, config_path: PathBuf, vault: VaultStore) -> Self {
        Self {
            config: Mutex::new(config),
            discovery_lock: Mutex::new(()),
            config_path,
            vault,
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

    // Mutations belong to services, not HTTP handlers. Read access deliberately
    // has no DerefMut; retaining a guard is necessary for account/catalog races.
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
        save_configuration_in_transaction(
            candidate,
            Some(&self.store.config_path),
            &self.store.vault,
            files,
        )?;
        load_configuration(Some(&self.store.config_path))
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
        let (saved, summary) = emp_state::migration::apply_migration_import(
            &self.current,
            decrypted,
            &self.store.config_path,
            &self.store.vault,
        )?;
        *self.current.0 = saved;
        Ok(summary)
    }
}
