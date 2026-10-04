use super::pipeline::{
    FetchAndPersistOutcome, PersistError, RefreshSource, fetch_and_persist, publish_pending_catalog,
};
use crate::app::ServerState;
use crate::services::accounts::duplicate_accounts;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(crate) fn request_refresh(state: &ServerState, force: bool) {
    state.catalog_refresh.request(force);
}

pub(crate) fn run(state: &ServerState) {
    loop {
        let Some(force) = state
            .catalog_refresh
            .wait_for_work(|| state.shutdown.load(Ordering::Acquire))
        else {
            return;
        };
        refresh_due_sources(state, force);
        state.catalog_refresh.finish_pass();
    }
}

fn refresh_due_sources(state: &ServerState, force: bool) {
    let Some(_poll) = state.catalog_refresh.poll_gate() else {
        return;
    };
    let sources = refresh_sources(state);
    let present = sources
        .iter()
        .map(|source| source.id.clone())
        .collect::<BTreeSet<_>>();
    for source in sources {
        if state.shutdown.load(Ordering::Acquire) {
            return;
        }
        let fingerprint = source.fingerprint();
        if !state.catalog_refresh.due(
            &source.id,
            &fingerprint,
            source.generation,
            force,
            Instant::now(),
        ) {
            continue;
        }
        match fetch_and_persist(state, &source) {
            FetchAndPersistOutcome::Persisted(_) => {
                state
                    .catalog_refresh
                    .completed(&source.id, true, false, Instant::now());
            }
            FetchAndPersistOutcome::UpstreamError(error) => {
                state.catalog_refresh.completed(
                    &source.id,
                    false,
                    matches!(error.status(), 401 | 403),
                    Instant::now(),
                );
            }
            FetchAndPersistOutcome::Stopped => return,
            FetchAndPersistOutcome::PersistenceError(
                PersistError::RuntimeChanged | PersistError::SourceChanged,
            ) => {
                // A delete, re-import, login switch or backend change raced
                // the request. The next scan gets the current source identity.
            }
            FetchAndPersistOutcome::PersistenceError(PersistError::Internal) => {
                state
                    .catalog_refresh
                    .completed(&source.id, false, false, Instant::now());
            }
        }
    }
    state.catalog_refresh.retain_sources(&present);

    let generated_catalog = crate::services::catalog::generated_catalog_path(state);
    if !generated_catalog.is_file() {
        state.catalog_refresh.mark_catalog_publication_pending();
    }
    let _ = publish_pending_catalog(state);
}

fn refresh_sources(state: &ServerState) -> Vec<RefreshSource> {
    let (config, generations) = match state.backend.configuration.read() {
        Ok(config) => {
            let config = config.clone();
            let generations = config
                .get("accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|account| account.get("id")?.as_str())
                .map(|id| (id.to_owned(), state.catalog_refresh.account_generation(id)))
                .collect::<BTreeMap<_, _>>();
            (config, generations)
        }
        Err(_) => return Vec::new(),
    };
    // ServerState configuration is normalized on load and after every save;
    // validate_codex_base_url applies the HTTPS/loopback and credential rules.
    let Some(base) = config
        .get("codex_base_url")
        .and_then(Value::as_str)
        .filter(|base| !base.is_empty())
    else {
        return Vec::new();
    };
    // Populate/refresh observations outside the configuration lock. The
    // persistence path must never prepare or launch an executable.
    state.backend.integration.inventory.snapshot(false);
    let Some(client_version) = state
        .backend
        .integration
        .inventory
        .selected_trusted_version()
    else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    if let Some(source) = RefreshSource::native(state, &config, base, client_version.clone()) {
        sources.push(source);
    }

    let duplicates = duplicate_accounts(
        &config,
        &state.backend.configuration.vault,
        &state.backend.accounts.native_auth_path,
    );
    let accounts = config
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut counts = BTreeMap::<String, usize>::new();
    for account in &accounts {
        if let Some(id) = account.get("id").and_then(Value::as_str) {
            *counts.entry(id.to_owned()).or_default() += 1;
        }
    }
    for account in accounts {
        if account.get("enabled") == Some(&Value::Bool(false)) {
            continue;
        }
        let Some(id) = account.get("id").and_then(Value::as_str) else {
            continue;
        };
        if counts.get(id) != Some(&1) || duplicates.contains_key(id) {
            continue;
        }
        let generation = generations.get(id).copied().unwrap_or_default();
        if let Some(source) = RefreshSource::account(
            state,
            &config,
            &account,
            base,
            client_version.clone(),
            generation,
            true,
        ) {
            sources.push(source);
        }
    }
    sources
}
