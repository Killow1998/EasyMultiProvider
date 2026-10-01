//! Shared explicit and opt-in startup activation; callers hold the operation lock.
use crate::app::ServerState;
use crate::services::accounts::native_auth_document;
use crate::services::catalog::{refresh_catalog, server_catalog};
use emp_integration::IntegrationResult;

pub(crate) enum EnableError {
    Unavailable(u16),
    EmptyCatalog,
}

pub(crate) fn apply(state: &ServerState) -> Result<IntegrationResult, EnableError> {
    let manager = &state.backend.integration.manager;
    let config = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| EnableError::Unavailable(503))?
        .clone();
    let catalog = server_catalog(state, &config);
    let visible = catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|model| {
            model
                .get("slug")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty())
                && model
                    .get("visibility")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("list")
                    == "list"
        });
    if !visible {
        return Err(EnableError::EmptyCatalog);
    }
    let (catalog_path, _) = refresh_catalog(state).map_err(|_| EnableError::Unavailable(503))?;
    let dynamic = native_auth_document(&state.backend.accounts.native_auth_path)
        .and_then(|auth| emp_state::validate_auth_json(&auth).ok())
        .is_some();
    let base_url = &state.base_url;
    let path = catalog_path.to_string_lossy();
    if dynamic {
        let status = manager
            .status()
            .map_err(|_| EnableError::Unavailable(409))?;
        if status.relation == "applied"
            && let Some(lease) = &status.lease
            && lease.fields["openai_base_url"].applied.value.as_deref() == Some(base_url)
            && (lease.fields["model_catalog_json"].applied.value.as_deref() == Some(path.as_ref())
                || (!lease.fields["model_catalog_json"].applied.present
                    && lease.fields[emp_integration::REALTIME_SIDEBAND_FIELD]
                        .applied
                        .value
                        .as_deref()
                        != Some(base_url)))
        {
            let result = manager
                .restore()
                .map_err(|_| EnableError::Unavailable(409))?;
            if !result.ok() {
                return Ok(result);
            }
        }
    }
    manager
        .enable_with_sideband(
            base_url,
            if dynamic { None } else { Some(path.as_ref()) },
            true,
            dynamic.then_some(base_url.as_str()),
        )
        .map_err(|_| EnableError::Unavailable(409))
}
