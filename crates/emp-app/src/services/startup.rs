//! Recover only an integration lease that targets this actual listener/catalog.
use crate::app::ServerState;
use crate::services::accounts::native_subscription_headers;
use crate::services::catalog::{generated_catalog_path, refresh_catalog};
use crate::services::integration::endpoint::is_previous_loopback_url;
use crate::services::runtime::mark_pending;
use std::sync::atomic::Ordering;

pub(crate) fn reconcile(state: &ServerState) -> Result<(), ()> {
    reconcile_with_base_url(state, &state.base_url)
}

fn reconcile_with_base_url(state: &ServerState, base_url: &str) -> Result<(), ()> {
    let integration = &state.backend.integration;
    let manager = &integration.manager;
    let _operation = manager.operation_lock().map_err(|_| ())?;
    if state.shutdown.load(Ordering::Acquire) {
        return Ok(());
    }
    let status = manager.status().map_err(|_| ())?;
    let catalog = generated_catalog_path(state);
    let catalog = emp_state::config::resolve_user_path(&catalog);
    if let Some(lease) = &status.lease
        && lease.status != "restored"
        && status.relation == "applied"
    {
        let mut conflicts = Vec::new();
        let previous = lease.fields["openai_base_url"].applied.value.as_deref();
        if previous != Some(base_url) && !is_previous_loopback_url(previous, base_url) {
            conflicts.push("listener_mismatch".to_owned());
        }
        if lease.fields["model_catalog_json"]
            .applied
            .value
            .as_deref()
            .is_some_and(|path| path != catalog.to_string_lossy())
        {
            conflicts.push("catalog_mismatch".to_owned());
        }
        if !conflicts.is_empty() {
            *integration.startup_conflicts.lock().map_err(|_| ())? = conflicts;
            return Ok(());
        }
    }
    let result = manager.recover(true, true).map_err(|_| ())?;
    if result.ok() && result.action == "re_adopted" && result.state == "active" {
        integration.owned.store(true, Ordering::Release);
        crate::services::integration::sync_features(state)?;
        integration
            .startup_conflicts
            .lock()
            .map_err(|_| ())?
            .clear();
        mark_pending(
            state,
            "emp",
            "EMP restarted; runtime catalog was not assumed",
        )?;
        refresh_catalog(state)?;
        let dynamic =
            native_subscription_headers(&state.backend.accounts.native_auth_path).is_some();
        if let Some(lease) = result.lease.as_ref()
            && (is_previous_loopback_url(
                lease.fields["openai_base_url"].applied.value.as_deref(),
                base_url,
            ) || dynamic
                && lease.fields["openai_base_url"].applied.value.as_deref() == Some(base_url)
                && (lease.fields["model_catalog_json"].applied.present
                    || lease.fields[emp_integration::REALTIME_SIDEBAND_FIELD]
                        .applied
                        .value
                        .as_deref()
                        != Some(base_url)))
        {
            let catalog = if dynamic {
                None
            } else {
                lease.fields["model_catalog_json"].applied.value.as_deref()
            };
            let sideband = (dynamic
                || is_previous_loopback_url(
                    lease.fields[emp_integration::REALTIME_SIDEBAND_FIELD]
                        .applied
                        .value
                        .as_deref(),
                    base_url,
                ))
            .then_some(base_url);
            if !manager.restore().map_err(|_| ())?.ok() {
                return Err(());
            }
            if !manager
                .enable_with_sideband(base_url, catalog, true, sideband)
                .map_err(|_| ())?
                .ok()
            {
                return Err(());
            }
            mark_pending(state, "emp", "EMP configuration applied")?;
        }
    }
    // Automatic activation may apply only to a clean native configuration. Recovery
    // conflicts and leases owned by another listener retain the existing guard.
    let auto_enable = state
        .backend
        .configuration
        .read()
        .map_err(|_| ())?
        .get("auto_enable_on_start")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let current = manager.status().map_err(|_| ())?;
    if auto_enable
        && result.ok()
        && matches!(current.state.as_str(), "native" | "restored")
        && matches!(current.relation.as_str(), "unleased" | "original")
    {
        let enabled = match super::integration::enable::apply(state) {
            Ok(enabled) => enabled,
            // A fresh install with no discovered models stays available for setup.
            Err(super::integration::enable::EnableError::EmptyCatalog) => return Ok(()),
            Err(super::integration::enable::EnableError::Unavailable(_)) => return Err(()),
        };
        if !enabled.ok() {
            return Err(());
        }
        integration.owned.store(true, Ordering::Release);
        if super::integration::sync_features(state).is_err() {
            if manager.restore().is_ok_and(|result| result.ok()) {
                let _ =
                    emp_integration::local_auth::restore(&state.backend.accounts.native_auth_path);
            }
            return Err(());
        }
        mark_pending(state, "emp", "EMP configuration applied")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
