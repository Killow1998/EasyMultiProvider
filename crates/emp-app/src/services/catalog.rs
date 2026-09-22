//! Catalog composition, persistence and identity.

use crate::app::ServerState;
use crate::services::accounts::account_catalog_headers;
use crate::services::accounts::duplicate_accounts;
use emp_codex::account_catalog;
use emp_codex::load_native_catalog;
use emp_codex::merged_catalog::build_catalog;
use emp_state::filesystem::write_catalog_json;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

pub(crate) fn refresh_catalog(state: &ServerState) -> Result<(PathBuf, usize), ()> {
    let config = state.backend.configuration.config.lock().map_err(|_| ())?;
    let catalog = server_catalog(state, &config);
    let path = generated_catalog_path(state);
    write_catalog_json(&path, &catalog).map_err(|_| ())?;
    drop(config);
    crate::services::runtime::mark_active_pending(state, "EMP model catalog changed");
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

pub(crate) struct CatalogSources {
    pub(crate) native: Value,
    pub(crate) accounts: BTreeMap<String, Value>,
    pub(crate) duplicates: BTreeMap<String, String>,
}

pub(crate) fn response_catalog_etag(state: &ServerState) -> Option<String> {
    let config = state.backend.configuration.config.lock().ok()?.clone();
    emp_state::catalog_etag(&server_catalog(state, &config)).ok()
}

pub(crate) fn server_catalog(state: &ServerState, config: &Value) -> Value {
    let sources = catalog_sources(state, config);
    build_catalog(
        config,
        &sources.native,
        &sources.accounts,
        &sources.duplicates,
    )
}

pub(crate) fn catalog_sources(state: &ServerState, config: &Value) -> CatalogSources {
    let native = load_native_catalog(config);
    let mut account_catalogs = BTreeMap::new();
    for account in config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(account_map) = account.as_object() else {
            continue;
        };
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let catalog = account_catalog(
            config.as_object().expect("config object"),
            account_map,
            &mut |account| account_catalog_headers(account, &state.backend.configuration.vault),
        );
        account_catalogs.insert(id.to_owned(), catalog);
    }
    CatalogSources {
        native,
        accounts: account_catalogs,
        duplicates: duplicate_accounts(
            config,
            &state.backend.configuration.vault,
            &state.backend.accounts.native_auth_path,
        ),
    }
}
