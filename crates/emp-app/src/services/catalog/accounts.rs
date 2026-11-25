//! Refresh one subscription's entitlements, checking ownership before persistence.
use super::{client_catalog, generated_catalog_path};
use crate::app::ServerState;
use emp_state::filesystem::write_catalog_json;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Instant};

mod pipeline;
mod refresh_state;
mod worker;

pub(super) use pipeline::catalog_headers;
use pipeline::{
    FetchAndPersistOutcome, PersistError, RefreshSource, fetch_and_persist, publish_pending_catalog,
};
use refresh_state::CatalogRefreshState;
pub(crate) use worker::run_refresh_worker;

#[derive(Default)]
pub(crate) struct CatalogState {
    refresh: CatalogRefreshState,
}

pub(crate) fn request_refresh(state: &ServerState, force: bool) {
    state.catalog.refresh.request(force);
}

pub(crate) fn account_source_changed(state: &ServerState, id: &str) {
    state.catalog.refresh.account_changed(id);
}

pub(crate) fn stop_refresh_worker(state: &ServerState) {
    state.catalog.refresh.stop();
}

pub(crate) fn refresh_catalog(state: &ServerState) -> Result<(PathBuf, usize), ()> {
    let config = state.backend.configuration.read().map_err(|_| ())?;
    state.catalog.refresh.mark_catalog_publication_pending();
    let catalog = client_catalog(state, &config);
    let path = generated_catalog_path(state);
    let previous = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let changed = previous.as_ref() != Some(&catalog);
    if changed {
        write_catalog_json(&path, &catalog).map_err(|_| ())?;
    }
    state.catalog.refresh.catalog_publication_succeeded();
    drop(config);
    if changed {
        crate::services::runtime::mark_active_pending(state, "EMP model catalog changed");
    }
    Ok((path, catalog["models"].as_array().map_or(0, Vec::len)))
}

#[derive(Debug)]
pub(crate) enum CatalogRefreshError {
    Invalid(&'static str),
    Internal,
    Upstream(emp_router::RouterError),
}
pub(crate) fn refresh_subscription(
    request_id: Option<&str>,
    state: &ServerState,
    id: &str,
) -> Result<Value, CatalogRefreshError> {
    crate::services::observation::operation::observe(
        &state.backend.diagnostics,
        request_id,
        "subscription_catalog_refresh",
        |receipt| {
            receipt.subject(id);
            receipt.check("runtime_catalog_matches_target", None);
            let _poll = state
                .catalog
                .refresh
                .poll_gate()
                .ok_or(CatalogRefreshError::Internal)?;
            let (config, generation) = {
                let current = state
                    .backend
                    .configuration
                    .read()
                    .map_err(|_| CatalogRefreshError::Internal)?;
                let generation = if id == "@native" {
                    0
                } else {
                    state.catalog.refresh.account_generation(id)
                };
                (current.clone(), generation)
            };
            receipt.number("source_generation", generation);
            let base = config["codex_base_url"]
                .as_str()
                .filter(|base| !base.is_empty())
                .ok_or(CatalogRefreshError::Invalid(
                    "Subscription backend is unavailable",
                ))?;
            let client_version = state
                .backend
                .integration
                .inventory
                .observe_selected_version()
                .ok_or(CatalogRefreshError::Invalid(
                    "Codex engine version is unavailable; model refresh was not started",
                ))?;
            let source = (if id == "@native" {
                RefreshSource::native(state, &config, base, client_version)
            } else {
                let account = config["accounts"]
                    .as_array()
                    .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
                    .ok_or(CatalogRefreshError::Invalid(
                        "Subscription account is unavailable",
                    ))?;
                RefreshSource::account(
                    state,
                    &config,
                    account,
                    base,
                    client_version,
                    generation,
                    false,
                )
            })
            .ok_or(CatalogRefreshError::Invalid(
                "Subscription credentials are unavailable",
            ))?;
            let persisted = receipt.step("fetch_and_persist", || {
                match fetch_and_persist(state, &source) {
                    FetchAndPersistOutcome::Persisted(persisted) => Ok(persisted),
                    FetchAndPersistOutcome::UpstreamError(error) => {
                        Err(CatalogRefreshError::Upstream(error))
                    }
                    FetchAndPersistOutcome::PersistenceError(error) => Err(match error {
                        PersistError::RuntimeChanged => CatalogRefreshError::Invalid(
                            "Codex runtime changed during model refresh; retry",
                        ),
                        PersistError::SourceChanged => CatalogRefreshError::Invalid(
                            "Subscription changed during model refresh; retry",
                        ),
                        PersistError::Internal => CatalogRefreshError::Internal,
                    }),
                    FetchAndPersistOutcome::Stopped => Err(CatalogRefreshError::Internal),
                }
            })?;
            receipt.check("account_catalog_persisted", Some(true));
            state.catalog.refresh.mark_fresh(
                &source.id,
                &source.fingerprint(),
                source.generation,
                Instant::now(),
            );
            receipt
                .step("publish_catalog", || publish_pending_catalog(state))
                .map_err(|_| CatalogRefreshError::Internal)?;
            receipt.check("catalog_published", Some(true));
            Ok(
                json!({"models":emp_codex::management_views::subscription_model_options(&persisted, false)}),
            )
        },
    )
}

#[cfg(all(test, unix))]
pub(super) mod tests;
