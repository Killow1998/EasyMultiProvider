//! Refresh one subscription's entitlements, checking ownership before persistence.
use crate::app::ServerState;
use serde_json::{Value, json};
use std::time::Instant;

mod pipeline;
mod refresh_state;
mod worker;

use pipeline::{
    FetchAndPersistOutcome, PersistError, RefreshSource, fetch_and_persist, publish_pending_catalog,
};
pub(crate) use refresh_state::CatalogRefreshState;
pub(crate) use worker::{request_refresh, run as run_refresh_worker};
#[derive(Debug)]
pub(crate) enum CatalogRefreshError {
    Invalid(&'static str),
    Internal,
    Upstream(emp_router::RouterError),
}
fn read_account_cache(path: &std::path::Path) -> Option<Value> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 4 * 1024 * 1024
    {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub(crate) fn refresh(
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
                .catalog_refresh
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
                    state.catalog_refresh.account_generation(id)
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
            // Version probing may prepare a signed desktop-engine snapshot. Do it
            // explicitly here, after releasing the configuration mutex; persistence
            // only reads the already verified observation and checks its identity.
            state.backend.integration.inventory.snapshot(false);
            let client_version = state
                .backend
                .integration
                .inventory
                .selected_trusted_version()
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
            state.catalog_refresh.mark_fresh(
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
                json!({"models":emp_codex::management_views::subscription_model_options(&persisted.catalog)}),
            )
        },
    )
}

#[cfg(all(test, unix))]
mod tests;
