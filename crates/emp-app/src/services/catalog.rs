//! Catalog composition, persistence and identity.

use crate::app::ServerState;
use crate::services::accounts::account_catalog_headers;
use emp_state::filesystem::write_catalog_json;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

pub(crate) mod discovery;
mod provider_models;
mod selection;
mod sources;
pub(crate) use selection::select_models;
use sources::CatalogSources;

pub(crate) fn refresh_catalog(state: &ServerState) -> Result<(PathBuf, usize), ()> {
    let config = state.backend.configuration.read().map_err(|_| ())?;
    state.catalog_refresh.mark_catalog_publication_pending();
    let catalog = client_catalog(state, &config);
    let path = generated_catalog_path(state);
    let previous = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let changed = previous.as_ref() != Some(&catalog);
    if changed {
        write_catalog_json(&path, &catalog).map_err(|_| ())?;
    }
    state.catalog_refresh.catalog_publication_succeeded();
    drop(config);
    if changed {
        crate::services::runtime::mark_active_pending(state, "EMP model catalog changed");
    }
    Ok((path, catalog["models"].as_array().map_or(0, Vec::len)))
}

pub(crate) fn generated_catalog_path(state: &ServerState) -> PathBuf {
    let home = state
        .backend
        .accounts
        .native_auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    emp_state::generated_catalog_path(Some(home))
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn response_catalog_etag(state: &ServerState) -> Option<String> {
    let config = state.backend.configuration.read().ok()?.clone();
    // The revision identifies the shared catalog, before client-version projection.
    let catalog = emp_state::config::client_catalog(&config, server_catalog(state, &config));
    emp_state::catalog_etag(&catalog).ok()
}

pub(crate) fn client_catalog(state: &ServerState, config: &Value) -> Value {
    let catalog = emp_state::config::client_catalog(config, server_catalog(state, config));
    emp_codex::merged_catalog::for_client_version(
        catalog,
        state
            .backend
            .integration
            .inventory
            .selected_trusted_version()
            .as_deref(),
    )
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn server_catalog(state: &ServerState, config: &Value) -> Value {
    let mut catalog = source_catalog(state, config);
    if state.backend.modules.active(emp_modules::Module::Aero) {
        super::modules::aero::augment_catalog(config, &mut catalog);
    }
    catalog
}

pub(crate) fn source_catalog(state: &ServerState, config: &Value) -> Value {
    let hide_aggregates = !state.backend.modules.active(emp_modules::Module::Aggregate)
        && config["model_groups"]
            .as_array()
            .is_some_and(|groups| !groups.is_empty());
    let unavailable = state.backend.team.unavailable_providers(
        state.backend.modules.active(emp_modules::Module::Team),
        config,
    );
    if hide_aggregates || !unavailable.is_empty() {
        let mut visible = config.clone();
        if hide_aggregates {
            visible["model_groups"] = serde_json::json!([]);
        }
        if !unavailable.is_empty() {
            visible["accounts"]
                .as_array_mut()
                .expect("normalized accounts")
                .retain(|account| !unavailable.iter().any(|id| account["id"] == *id));
            visible["models"]
                .as_array_mut()
                .expect("normalized models")
                .retain(|model| {
                    !unavailable
                        .iter()
                        .any(|provider| model["provider"] == *provider)
                });
        }
        catalog_sources(state, &visible).merged(&visible)
    } else {
        catalog_sources(state, config).merged(config)
    }
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn catalog_sources(state: &ServerState, config: &Value) -> CatalogSources {
    CatalogSources::load(state, config)
}

pub(crate) fn public_configuration(
    state: &ServerState,
    config: &Value,
) -> Result<Value, emp_state::ConfigError> {
    let mut public = catalog_sources(state, config).public_configuration(state, config)?;
    if state.backend.modules.active(emp_modules::Module::Aero) {
        public["aero_profiles"] = config
            .get("aero_profiles")
            .cloned()
            .unwrap_or(serde_json::json!([]));
        super::modules::aero::augment_public(config, &mut public);
    }
    Ok(public)
}

pub(crate) fn subscription_options(
    state: &ServerState,
    config: &Value,
    id: &str,
) -> Result<Vec<Value>, &'static str> {
    let catalog = if id == "@native" {
        Some(emp_codex::load_native_catalog(config))
    } else {
        let account = config["accounts"]
            .as_array()
            .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
            .filter(|account| {
                account["auth_file"]
                    .as_str()
                    .is_some_and(|path| !path.is_empty())
            })
            .and_then(Value::as_object)
            .ok_or("Subscription account is unavailable")?;
        emp_codex::cached_account_catalog(
            config.as_object().expect("config"),
            account,
            &mut |account| account_catalog_headers(account, &state.backend.configuration.vault),
        )
    };
    Ok(catalog.as_ref().map_or_else(Vec::new, |catalog| {
        emp_codex::management_views::subscription_model_options(catalog, false)
    }))
}

pub(crate) fn subscription_model(
    state: &ServerState,
    config: &serde_json::Map<String, Value>,
    slug: &str,
    account: Option<&serde_json::Map<String, Value>>,
) -> Option<serde_json::Map<String, Value>> {
    emp_codex::subscription_route_model(config, slug, account, |account| {
        account_catalog_headers(account, &state.backend.configuration.vault)
    })
}

/// The existing refresh button publishes locally, then schedules subscription
/// refresh. Scheduling is not evidence of background completion or client load.
pub(crate) fn refresh_for_management(
    request_id: Option<&str>,
    state: &ServerState,
) -> Result<(PathBuf, usize), ()> {
    crate::services::observation::operation::observe(
        &state.backend.diagnostics,
        request_id,
        "catalog_refresh",
        |receipt| {
            let result = receipt.step("publish_catalog", || refresh_catalog(state))?;
            receipt.check("catalog_published", Some(true));
            crate::services::account_catalog::request_refresh(state, false);
            receipt.fact("subscription_refresh", "scheduled");
            receipt.check("runtime_catalog_matches_target", None);
            Ok(result)
        },
    )
}
