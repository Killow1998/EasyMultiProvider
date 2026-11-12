//! Shared explicit and opt-in startup activation; callers hold the operation lock.
use crate::app::ServerState;
use crate::services::accounts::native_subscription_headers;
use crate::services::catalog::{refresh_catalog, server_catalog};
use emp_integration::IntegrationResult;

pub(crate) fn image_mcp_required(auth: &serde_json::Value) -> bool {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    if emp_integration::local_auth::is_local(auth) {
        return true;
    }
    auth["tokens"]["id_token"]
        .as_str()
        .and_then(|token| token.split('.').nth(1))
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok())
        .and_then(|payload| serde_json::from_slice::<serde_json::Value>(&payload).ok())
        .is_some_and(|claims| claims["https://api.openai.com/auth"]["chatgpt_plan_type"] == "free")
}

pub(crate) enum EnableError {
    Unavailable(u16),
    EmptyCatalog,
}

pub(crate) fn apply(state: &ServerState) -> Result<IntegrationResult, EnableError> {
    let manager = &state.backend.integration.manager;
    let catalog = {
        let config = state
            .backend
            .configuration
            .read()
            .map_err(|_| EnableError::Unavailable(503))?;
        server_catalog(state, &config)
    };
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
    // Subscription authentication uses Codex's online model discovery.
    // API-only engines retain the catalog-file compatibility fallback.
    let dynamic = native_subscription_headers(&state.backend.accounts.native_auth_path).is_some();
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
    let auth_path = &state.backend.accounts.native_auth_path;
    let created = emp_integration::local_auth::ensure(auth_path, &state.backend.gateway_key)
        .map_err(|_| EnableError::Unavailable(409))?;
    if created && refresh_catalog(state).is_err() {
        emp_integration::local_auth::restore(auth_path)
            .map_err(|_| EnableError::Unavailable(409))?;
        return Err(EnableError::Unavailable(503));
    }
    let auth = emp_integration::local_auth::read(auth_path)
        .map_err(|_| EnableError::Unavailable(409))?
        .ok_or(EnableError::Unavailable(409))?;
    let image_mcp = if image_mcp_required(&auth) {
        Some(emp_integration::ImageMcpConfig {
            command: std::env::current_exe()
                .map_err(|_| EnableError::Unavailable(409))?
                .to_string_lossy()
                .into_owned(),
            key_file: state
                .backend
                .configuration
                .config_path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .join("state/gateway-key")
                .to_string_lossy()
                .into_owned(),
        })
    } else {
        None
    };
    let result = manager
        .enable_with_options(
            base_url,
            if dynamic { None } else { Some(path.as_ref()) },
            true,
            dynamic.then_some(base_url.as_str()),
            image_mcp.as_ref(),
        )
        .map_err(|_| EnableError::Unavailable(409));
    if created && !result.as_ref().is_ok_and(|result| result.ok()) {
        emp_integration::local_auth::restore(auth_path)
            .map_err(|_| EnableError::Unavailable(409))?;
    }
    result
}
