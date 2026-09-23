//! Recover only an integration lease that targets this actual listener/catalog.
use crate::app::ServerState;
use crate::services::accounts::native_auth_document;
use crate::services::catalog::{generated_catalog_path, refresh_catalog};
use crate::services::runtime::mark_pending;
use std::sync::atomic::Ordering;

pub(crate) fn reconcile(state: &ServerState) -> Result<(), ()> {
    let integration = &state.backend.integration;
    let manager = &integration.manager;
    let _operation = manager.operation_lock().map_err(|_| ())?;
    let status = manager.status().map_err(|_| ())?;
    let catalog = generated_catalog_path(state);
    let catalog = emp_state::config::resolve_user_path(&catalog);
    if let Some(lease) = &status.lease
        && lease.status != "restored"
        && status.relation == "applied"
    {
        let mut conflicts = Vec::new();
        if lease.fields["openai_base_url"].applied.value.as_deref() != Some(&state.base_url) {
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
        crate::services::integration::sync_search(state)?;
        integration.owned.store(true, Ordering::Release);
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
        let dynamic = native_auth_document(&state.backend.accounts.native_auth_path)
            .and_then(|auth| emp_state::validate_auth_json(&auth).ok())
            .is_some();
        if dynamic
            && result
                .lease
                .as_ref()
                .is_some_and(|lease| lease.fields["model_catalog_json"].applied.present)
        {
            if !manager.restore().map_err(|_| ())?.ok() {
                return Err(());
            }
            if !manager
                .enable(&state.base_url, None, true)
                .map_err(|_| ())?
                .ok()
            {
                return Err(());
            }
            mark_pending(state, "emp", "EMP configuration applied")?;
            crate::services::runtime::sync_runtime(state, Some("emp"), false, false, false)
                .map_err(|_| ())?;
        }
    }
    Ok(())
}
