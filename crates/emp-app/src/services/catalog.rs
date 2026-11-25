use crate::app::ServerState;
use crate::modules::hooks::{HookOperation, HookPhase};
use crate::services::accounts::subscription_auth_headers;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

mod accounts;
mod discovery;
mod provider_models;
mod selection;
mod sources;
mod validation;
#[cfg(all(test, unix))]
pub(crate) use accounts::tests::assert_subscription_refresh_receipt;
pub(crate) use accounts::{
    CatalogRefreshError, CatalogState, account_source_changed, refresh_catalog,
    refresh_subscription, request_refresh, run_refresh_worker, stop_refresh_worker,
};
pub(crate) use discovery::{
    DiscoveryError, ProviderDiscovery, discover, metadata as model_metadata,
    saved_models as saved_provider_models,
};
use sources::CatalogSources;
pub(super) use validation::validate_update;

pub(crate) fn generated_catalog_path(state: &ServerState) -> PathBuf {
    let home = state
        .backend
        .codex
        .auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    emp_state::generated_catalog_path(Some(home))
}

#[cfg_attr(feature = "hotpath", hotpath::measure)]
pub(crate) fn response_catalog_etag(state: &ServerState) -> Option<String> {
    let config = state.backend.configuration.read().ok()?.clone();
    let shared_catalog = emp_state::config::client_catalog(&config, server_catalog(state, &config));
    emp_state::catalog_etag(&shared_catalog).ok()
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
    CatalogSources::load(state, &mut visible).merged(&visible)
}

pub(crate) fn public_configuration(
    state: &ServerState,
    config: &Value,
    hooks: Option<&HookOperation>,
) -> Value {
    let mut effective = config.clone();
    let sources = CatalogSources::load(state, &mut effective);
    let mut public = sources.public_configuration(state, &effective);
    if state.backend.modules.active(emp_modules::Module::Aero) {
        public["aero_profiles"] = config
            .get("aero_profiles")
            .cloned()
            .unwrap_or(serde_json::json!([]));
        super::modules::aero::augment_public(config, &mut public);
    }
    if project_catalog(hooks, &mut public["catalog_models"], "id") {
        public["catalog_families"] = json!(emp_codex::management_views::catalog_families(
            config,
            public["catalog_models"]
                .as_array()
                .expect("public catalog models"),
        ));
    }
    public
}

pub(crate) fn project_catalog(
    hooks: Option<&HookOperation>,
    entries: &mut Value,
    id: &str,
) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        id: String,
        display_name: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Projection {
        entries: Vec<Entry>,
        hidden_ids: Vec<String>,
    }
    let Some(hooks) = hooks.filter(|hooks| hooks.has(HookPhase::ProjectCatalog)) else {
        return false;
    };
    let entries = entries.as_array_mut().expect("catalog models");
    let mut ids: BTreeSet<_> = entries
        .iter()
        .filter_map(|entry| entry[id].as_str().map(str::to_owned))
        .collect();
    let base_ids = ids.clone();
    let mut hidden = BTreeSet::new();
    let mut changed = false;
    let _ = hooks.dispatch::<Projection, ()>(
        HookPhase::ProjectCatalog,
        || json!({"model_ids":base_ids}),
        |module, projection| {
            let mut added = BTreeSet::new();
            if (!module.catalog && !projection.entries.is_empty())
                || projection.entries.iter().any(|entry| {
                    !entry.id.starts_with(&format!("{}/", module.id))
                        || ids.contains(&entry.id)
                        || !added.insert(entry.id.clone())
                })
            {
                return Err(());
            }
            changed |= !added.is_empty();
            ids.extend(added);
            entries.extend(
                projection
                    .entries
                    .into_iter()
                    .map(|entry| json!({(id):entry.id, "display_name":entry.display_name})),
            );
            hidden.extend(projection.hidden_ids);
            Ok(None)
        },
    );
    let before = entries.len();
    entries.retain(|entry| entry[id].as_str().is_none_or(|id| !hidden.contains(id)));
    changed || before != entries.len()
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
            .ok_or("Subscription account is unavailable")?;
        let auth_path = account["auth_file"]
            .as_str()
            .filter(|path| !path.is_empty())
            .ok_or("Subscription account is unavailable")?;
        emp_codex::cached_subscription_catalog(
            config.as_object().expect("config"),
            auth_path,
            || accounts::catalog_headers(state, config, account),
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
    let catalog_auth_path =
        account.and_then(|account| account.get("_emp_catalog_auth_file")?.as_str());
    emp_codex::subscription_route_model(config, slug, account, catalog_auth_path, |account| {
        subscription_auth_headers(account, &state.backend.configuration.vault)
    })
}

/// Publish locally, then schedule subscription refresh without claiming completion or client load.
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
            request_refresh(state, false);
            receipt.fact("subscription_refresh", "scheduled");
            receipt.check("runtime_catalog_matches_target", None);
            Ok(result)
        },
    )
}
