//! Restore this service's configuration before its listener goes away.
use super::IntegrationState;
use crate::error::AppError;
use emp_integration::IntegrationManager;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) fn restore_native_configuration(
    manager: &IntegrationManager,
    search: &emp_integration::search::SearchFeatureManager,
    preferences: &emp_integration::preferences::CodexPreferences,
    home: &std::path::Path,
) -> Result<(emp_integration::IntegrationResult, serde_json::Value), &'static str> {
    if manager
        .status()
        .map_err(|_| "integration_status_unavailable")?
        .relation
        == "other"
    {
        let result = manager
            .restore()
            .map_err(|_| "native_configuration_restore_failed")?;
        return Ok((result, serde_json::Value::Null));
    }
    let migration = emp_codex::history::repair::repair_idle_before_native_restore(home);
    let (history_lock, report) = match migration {
        Ok((lock, report)) => (
            Some(lock),
            serde_json::json!({"status":if report.threads_deferred == 0 {"complete"} else {"partial"}, "report":report}),
        ),
        Err(error) => (
            None,
            serde_json::json!({"status":"deferred", "reason":error.reason()}),
        ),
    };
    let path = manager.lease_path().with_file_name("history-restore.json");
    if let Ok(bytes) = serde_json::to_vec(&serde_json::json!({"codex_home":home,"outcome":report}))
        && std::fs::read(&path).ok().as_deref() != Some(bytes.as_slice())
    {
        let _ = emp_state::atomic_write_private_state(&path, &bytes);
    }
    search
        .restore()
        .map_err(|_| "search_configuration_restore_failed")?;
    preferences
        .restore()
        .map_err(|_| "compaction_configuration_restore_failed")?;
    let result = manager
        .restore()
        .map_err(|_| "native_configuration_restore_failed")?;
    drop(history_lock);
    Ok((result, report))
}

pub(super) fn read_history_restore(
    manager: &IntegrationManager,
    home: &std::path::Path,
) -> serde_json::Value {
    let saved = std::fs::read(manager.lease_path().with_file_name("history-restore.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_default();
    if saved["codex_home"].as_str() == home.to_str()
        && matches!(
            saved["outcome"]["status"].as_str(),
            Some("partial" | "deferred" | "complete")
        )
    {
        saved["outcome"].clone()
    } else {
        serde_json::Value::Null
    }
}

impl IntegrationState {
    pub(crate) fn restore_native_configuration(
        &self,
        manager: &IntegrationManager,
    ) -> Result<emp_integration::IntegrationResult, &'static str> {
        let (result, report) = restore_native_configuration(
            manager,
            &self.search,
            &self.preferences,
            &self.codex_home,
        )?;
        if let Ok(mut saved) = self.history_restore.lock() {
            *saved = report;
        }
        Ok(result)
    }

    pub(crate) fn restore_owned(&self) -> Result<(), AppError> {
        self.restore_owned_with(&self.manager, false)
    }

    pub(crate) fn restore_owned_on_exit(&self) -> Result<(), AppError> {
        // Three short lock acquisitions (operation, status, restore) leave
        // room for local writes within Windows' console-close deadline.
        let manager = self
            .manager
            .clone()
            .with_lock_timeout(Duration::from_millis(400));
        self.restore_owned_with(&manager, true)
    }

    fn restore_owned_with(
        &self,
        manager: &IntegrationManager,
        exiting: bool,
    ) -> Result<(), AppError> {
        let _operation = manager
            .operation_lock()
            .map_err(|_| AppError::NativeRestoreBlocked("integration_busy"))?;
        // An enable admitted before shutdown may have finished while we were
        // waiting. Check ownership inside the same lock as all config writes.
        let owned = self.owned.load(Ordering::Acquire);
        if !owned && !exiting {
            return Ok(());
        }
        let status = manager
            .status()
            .map_err(|_| AppError::NativeRestoreBlocked("integration_status_unavailable"))?;
        if !status.same_instance {
            return if owned {
                Err(AppError::NativeRestoreBlocked("integration_owner_changed"))
            } else {
                Ok(())
            };
        }
        // Enable can unwind after writing its lease but before publishing the
        // in-memory flag. The persisted instance identity is authoritative.
        let search = if exiting {
            // A user edit to optional search settings must not prevent removal
            // of our routing override. Attempt both restores before returning
            // the search error; both managers still compare ownership.
            self.search.restore()
        } else {
            Ok(())
        };
        let compaction = if exiting {
            self.preferences.restore()
        } else {
            Ok(())
        };
        let result = if exiting {
            manager
                .restore_owned_fields()
                .map_err(|_| "native_configuration_restore_failed")
        } else {
            self.restore_native_configuration(manager)
        }
        .map_err(AppError::NativeRestoreBlocked)?;
        if !result.ok() {
            return Err(AppError::NativeRestoreBlocked(
                "native_configuration_conflict",
            ));
        }
        search
            .map_err(|_| AppError::NativeRestoreBlocked("search_configuration_restore_failed"))?;
        compaction.map_err(|_| {
            AppError::NativeRestoreBlocked("compaction_configuration_restore_failed")
        })?;
        self.owned.store(false, Ordering::Release);
        Ok(())
    }
}
