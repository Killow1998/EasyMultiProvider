//! Refresh one subscription's entitlements, checking ownership before persistence.
use crate::app::ServerState;
use crate::http::response::{json_error_response, status_text};
use serde_json::{Value, json};
use std::time::Instant;

mod pipeline;
mod refresh_state;
mod worker;

use crate::services::failures::router_error_response;
use pipeline::{
    FetchAndPersistOutcome, PersistError, RefreshSource, fetch_and_persist, publish_pending_catalog,
};
pub(crate) use refresh_state::CatalogRefreshState;
pub(crate) use worker::{request_refresh, run as run_refresh_worker};
fn invalid(message: &str) -> Vec<u8> {
    json_error_response(400, status_text(400), message, None, &[])
}
fn internal() -> Vec<u8> {
    json_error_response(500, status_text(500), "internal server error", None, &[])
}
fn read_account_cache(path: &std::path::Path) -> Option<Value> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 4 * 1024 * 1024
    {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub(crate) fn refresh(state: &ServerState, id: &str) -> Result<Value, Vec<u8>> {
    let _poll = state.catalog_refresh.poll_gate().ok_or_else(internal)?;
    let (config, generation) = {
        let current = state
            .backend
            .configuration
            .config
            .lock()
            .map_err(|_| internal())?;
        let generation = if id == "@native" {
            0
        } else {
            state.catalog_refresh.account_generation(id)
        };
        (current.clone(), generation)
    };
    let base = config["codex_base_url"]
        .as_str()
        .filter(|base| !base.is_empty())
        .ok_or_else(|| invalid("Subscription backend is unavailable"))?;
    let client_version = state
        .backend
        .integration
        .inventory
        .selected_trusted_version()
        .ok_or_else(|| {
            invalid("Codex engine version is unavailable; model refresh was not started")
        })?;
    let source = (if id == "@native" {
        RefreshSource::native(state, &config, base, client_version)
    } else {
        let account = config["accounts"]
            .as_array()
            .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
            .ok_or_else(|| invalid("Subscription account is unavailable"))?;
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
    .ok_or_else(|| invalid("Subscription credentials are unavailable"))?;
    let persisted = match fetch_and_persist(state, &source) {
        FetchAndPersistOutcome::Persisted(persisted) => persisted,
        FetchAndPersistOutcome::UpstreamError(error) => {
            return Err(router_error_response(error));
        }
        FetchAndPersistOutcome::PersistenceError(error) => {
            return Err(match error {
                PersistError::RuntimeChanged => {
                    invalid("Codex runtime changed during model refresh; retry")
                }
                PersistError::SourceChanged => {
                    invalid("Subscription changed during model refresh; retry")
                }
                PersistError::Internal => internal(),
            });
        }
        FetchAndPersistOutcome::Stopped => return Err(internal()),
    };
    state.catalog_refresh.mark_fresh(
        &source.id,
        &source.fingerprint(),
        source.generation,
        Instant::now(),
    );
    publish_pending_catalog(state).map_err(|_| internal())?;
    Ok(
        json!({"models":emp_codex::management_views::subscription_model_options(&persisted.catalog)}),
    )
}

#[cfg(all(test, unix))]
mod tests;
