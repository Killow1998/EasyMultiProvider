//! Restore this service's configuration before its listener goes away.
use super::{IntegrationState, restore_native_configuration};
use crate::error::AppError;
use emp_integration::IntegrationManager;
use std::sync::atomic::Ordering;
use std::time::Duration;

impl IntegrationState {
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
        let result = if exiting {
            manager
                .restore_owned_fields()
                .map_err(|_| "native_configuration_restore_failed")
        } else {
            restore_native_configuration(manager, Some(&self.search))
        }
        .map_err(AppError::NativeRestoreBlocked)?;
        if !result.ok() {
            return Err(AppError::NativeRestoreBlocked(
                "native_configuration_conflict",
            ));
        }
        search
            .map_err(|_| AppError::NativeRestoreBlocked("search_configuration_restore_failed"))?;
        self.owned.store(false, Ordering::Release);
        Ok(())
    }
}
